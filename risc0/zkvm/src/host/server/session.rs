// Copyright 2023 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This module defines [Session] and [Segment] which provides a way to share
//! execution traces between the execution phase and the proving phase.

use alloc::collections::BTreeSet;
use std::{
    borrow::Borrow,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{anyhow, ensure, Result};
use serde::{Deserialize, Serialize};

use crate::{
    host::server::exec::executor::SyscallRecord,
    receipt_metadata::{Assumptions, Output},
    sha::Digest,
    Assumption, ExitCode, Journal, MemoryImage, ReceiptMetadata, SystemState,
};

#[derive(Clone, Default, Serialize, Deserialize, Debug)]
pub struct PageFaults {
    pub(crate) reads: BTreeSet<u32>,
    pub(crate) writes: BTreeSet<u32>,
}

/// The execution trace of a program.
///
/// The record of memory transactions of an execution that starts from an
/// initial memory image (which includes the starting PC) and proceeds until
/// either a sys_halt or a sys_pause syscall is encountered. This record is
/// stored as a vector of [Segment]s.
#[derive(Serialize, Deserialize)]
pub struct Session {
    /// The constituent [Segment]s of the Session. The final [Segment] will have
    /// an [ExitCode] of [Halted](ExitCode::Halted), [Paused](ExitCode::Paused),
    /// or [SessionLimit](ExitCode::SessionLimit), and all other [Segment]s (if
    /// any) will have [ExitCode::SystemSplit].
    pub segments: Vec<Box<dyn SegmentRef>>,

    /// The data publicly committed by the guest program.
    pub journal: Option<Journal>,

    /// The [ExitCode] of the session.
    pub exit_code: ExitCode,

    /// The final [MemoryImage] at the end of execution.
    pub post_image: MemoryImage,

    /// The list of assumptions made by the guest and resolved by the host.
    pub assumptions: Vec<Assumption>,

    /// The hooks to be called during the proving phase.
    #[serde(skip)]
    pub hooks: Vec<Box<dyn SessionEvents>>,
}

/// A reference to a [Segment].
///
/// This allows implementors to determine the best way to represent this in an
/// pluggable manner. See the [SimpleSegmentRef] for a very basic
/// implmentation.
#[typetag::serde(tag = "type")]
pub trait SegmentRef: Send {
    /// Resolve this reference into an actual [Segment].
    fn resolve(&self) -> Result<Segment>;
}

/// The execution trace of a portion of a program.
///
/// The record of memory transactions of an execution that starts from an
/// initial memory image, and proceeds until terminated by the system or user.
/// This represents a chunk of execution work that will be proven in a single
/// call to the ZKP system. It does not necessarily represent an entire program;
/// see [Session] for tracking memory transactions until a user-requested
/// termination.
#[derive(Clone, Serialize, Deserialize)]
pub struct Segment {
    pub(crate) pre_image: Box<MemoryImage>,
    pub(crate) post_image_id: Digest,
    pub(crate) faults: PageFaults,
    pub(crate) syscalls: Vec<SyscallRecord>,
    pub(crate) split_insn: Option<u32>,
    pub(crate) exit_code: ExitCode,

    /// The number of cycles in powers of 2.
    pub po2: u32,

    /// The index of this [Segment] within the [Session]
    pub index: u32,

    /// The number of user cycles without any overhead for continuations or po2
    /// padding.
    pub cycles: u32,
}

/// The Events of [Session]
pub trait SessionEvents {
    /// Fired before the proving of a segment starts.
    #[allow(unused)]
    fn on_pre_prove_segment(&self, segment: &Segment) {}

    /// Fired after the proving of a segment ends.
    #[allow(unused)]
    fn on_post_prove_segment(&self, segment: &Segment) {}
}

impl Session {
    /// Construct a new [Session] from its constituent components.
    pub fn new(
        segments: Vec<Box<dyn SegmentRef>>,
        journal: Option<Vec<u8>>,
        exit_code: ExitCode,
        post_image: MemoryImage,
        assumptions: Vec<Assumption>,
    ) -> Self {
        Self {
            segments,
            journal: journal.map(|x| Journal::new(x)),
            exit_code,
            post_image,
            assumptions,
            hooks: Vec::new(),
        }
    }

    /// A convenience method that resolves all [SegmentRef]s and returns the
    /// associated [Segment]s.
    pub fn resolve(&self) -> Result<Vec<Segment>> {
        self.segments
            .iter()
            .map(|segment_ref| segment_ref.resolve())
            .collect()
    }

    /// Add a hook to be called during the proving phase.
    pub fn add_hook<E: SessionEvents + 'static>(&mut self, hook: E) {
        self.hooks.push(Box::new(hook));
    }

    /// Calculate for the [ReceiptMetadata] associated with this [Session]. The
    /// [ReceiptMetadata] is the claim that will be proven if this [Session]
    /// is passed to the [crate::Prover].
    pub fn get_metadata(&self) -> Result<ReceiptMetadata> {
        let first_segment = &self
            .segments
            .first()
            .ok_or_else(|| anyhow!("session has no segments"))?
            .resolve()?;
        let last_segment = &self
            .segments
            .last()
            .ok_or_else(|| anyhow!("session has no segments"))?
            .resolve()?;

        // Construct the Output struct, checking that the Session is internally
        // consistent.
        let output = if self.exit_code.expects_output() {
            self.journal
                .as_ref()
                .map(|journal| -> Result<_> {
                    Ok(Output {
                        journal: journal.bytes.clone().into(),
                        assumptions: Assumptions(
                            self.assumptions
                                .iter()
                                .filter_map(|a| match a {
                                    Assumption::Proven(_) => None,
                                    Assumption::Unresolved(r) => Some(r.clone()),
                                })
                                .collect::<Vec<_>>(),
                        )
                        .into(),
                    })
                })
                .transpose()?
        } else {
            ensure!(
                self.journal.is_none(),
                "Session with exit code {:?} has a journal",
                self.exit_code
            );
            ensure!(
                self.assumptions.is_empty(),
                "Session with exit code {:?} has encoded assumptions",
                self.exit_code
            );
            None
        };

        // NOTE: When a segment ends in a Halted(_) state, it may not update the post state
        // digest. As a result, it will be the same are the pre_image. All other exit codes require
        // the post state digest to reflect the final memory state.
        let post_state = SystemState {
            pc: self.post_image.pc,
            merkle_root: match self.exit_code {
                ExitCode::Halted(_) => last_segment.pre_image.compute_root_hash(),
                _ => self.post_image.compute_root_hash(),
            },
        };

        Ok(ReceiptMetadata {
            pre: SystemState::from(first_segment.pre_image.borrow()).into(),
            post: post_state.into(),
            exit_code: self.exit_code,
            input: Digest::ZERO,
            output: output.into(),
        })
    }

    /// Report cycle information for this [Session].
    ///
    /// Returns a tuple `(x, y)` where:
    /// * `x`: Total number of cycles that a prover experiences. This includes
    ///   overhead associated with continuations and padding up to the nearest
    ///   power of 2.
    /// * `y`: Total number of cycles used for executing user instructions.
    pub fn get_cycles(&self) -> Result<(u64, u64)> {
        let segments = self.resolve()?;
        Ok(segments
            .iter()
            .fold((0, 0), |(total_cycles, user_cycles), segment| {
                (
                    total_cycles + (1 << segment.po2),
                    user_cycles + segment.cycles as u64,
                )
            }))
    }
}

impl Segment {
    /// Create a new [Segment] from its constituent components.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        pre_image: Box<MemoryImage>,
        post_image_id: Digest,
        faults: PageFaults,
        syscalls: Vec<SyscallRecord>,
        exit_code: ExitCode,
        split_insn: Option<u32>,
        po2: u32,
        index: u32,
        cycles: u32,
    ) -> Self {
        log::info!("segment[{index}]> reads: {}, writes: {}, exit_code: {exit_code:?}, split_insn: {split_insn:?}, po2: {po2}, cycles: {cycles}",
            faults.reads.len(),
            faults.writes.len(),
        );
        Self {
            pre_image,
            post_image_id,
            faults,
            syscalls,
            exit_code,
            split_insn,
            po2,
            index,
            cycles,
        }
    }
}

/// A very basic implementation of a [SegmentRef].
///
/// The [Segment] itself is stored in this implementation.
#[derive(Clone, Serialize, Deserialize)]
pub struct SimpleSegmentRef {
    segment: Segment,
}

#[typetag::serde]
impl SegmentRef for SimpleSegmentRef {
    fn resolve(&self) -> Result<Segment> {
        Ok(self.segment.clone())
    }
}

impl SimpleSegmentRef {
    /// Construct a [SimpleSegmentRef] with the specified [Segment].
    pub fn new(segment: Segment) -> Self {
        Self { segment }
    }
}

/// A basic implementation of a [SegmentRef] that saves the segment to a file
///
/// The [Segment] is stored in a user-specified file in this implementation,
/// and the SegmentRef holds the filename.
///
/// There is an example of using [FileSegmentRef] in our [EVM example][1]
///
/// [1]: https://github.com/risc0/risc0/blob/main/examples/zkevm-demo/src/main.rs
#[derive(Clone, Serialize, Deserialize)]
pub struct FileSegmentRef {
    path: PathBuf,
}

#[typetag::serde]
impl SegmentRef for FileSegmentRef {
    fn resolve(&self) -> Result<Segment> {
        let mut contents = Vec::new();
        let mut file = File::open(&self.path)?;
        file.read_to_end(&mut contents)?;
        let segment: Segment = bincode::deserialize(&contents)?;
        Ok(segment)
    }
}

impl FileSegmentRef {
    /// Construct a [FileSegmentRef]
    ///
    /// This builds a FileSegmentRef that stores `segment` in a file at `path`.
    pub fn new(segment: &Segment, path: &Path) -> Result<Self> {
        let path = path.join(format!("{}.bincode", segment.index));
        let mut file = File::create(&path)?;
        let contents = bincode::serialize(&segment)?;
        file.write_all(&contents)?;
        Ok(Self { path })
    }
}

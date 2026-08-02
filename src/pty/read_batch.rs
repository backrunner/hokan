use std::io::{self, Read};

use crate::terminal::{ChildOutputBatch, DrainState};

const DEFAULT_CHUNK_SIZE: usize = 16 * 1024;
const DEFAULT_CYCLE_BUDGET: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadCycleState {
    DrainedToEagain,
    BudgetExhausted,
    Eof,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadCycleOutcome {
    pub read_cycle: u64,
    pub batches: Vec<ChildOutputBatch>,
    pub state: ReadCycleState,
}

#[derive(Debug)]
pub struct NonblockingBatchReader {
    read_cycle: u64,
    chunk_size: usize,
    cycle_budget: usize,
}

impl Default for NonblockingBatchReader {
    fn default() -> Self {
        Self::new(DEFAULT_CHUNK_SIZE, DEFAULT_CYCLE_BUDGET)
    }
}

impl NonblockingBatchReader {
    #[must_use]
    pub fn new(chunk_size: usize, cycle_budget: usize) -> Self {
        let chunk_size = chunk_size.max(1);
        Self {
            read_cycle: 0,
            chunk_size,
            cycle_budget: cycle_budget.max(chunk_size),
        }
    }

    pub fn read_cycle<R: Read>(&mut self, reader: &mut R) -> io::Result<ReadCycleOutcome> {
        self.read_cycle = self
            .read_cycle
            .checked_add(1)
            .ok_or_else(|| io::Error::other("PTY read cycle exhausted"))?;
        let mut chunks = Vec::new();
        let mut total = 0usize;
        let state = loop {
            if total >= self.cycle_budget {
                break ReadCycleState::BudgetExhausted;
            }
            let remaining = self.cycle_budget - total;
            let mut buffer = vec![0; self.chunk_size.min(remaining)];
            match reader.read(&mut buffer) {
                Ok(0) => break ReadCycleState::Eof,
                Ok(count) => {
                    buffer.truncate(count);
                    total += count;
                    chunks.push(buffer);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    break ReadCycleState::DrainedToEagain;
                }
                Err(error) => return Err(error),
            }
        };

        if chunks.is_empty() && state == ReadCycleState::DrainedToEagain {
            chunks.push(Vec::new());
        }
        let last_index = chunks.len().saturating_sub(1);
        let batches = chunks
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| ChildOutputBatch {
                read_cycle: self.read_cycle,
                bytes,
                drain: if state == ReadCycleState::DrainedToEagain && index == last_index {
                    DrainState::DrainedToEagain
                } else {
                    DrainState::MoreInCurrentCycle
                },
            })
            .collect();

        Ok(ReadCycleOutcome {
            read_cycle: self.read_cycle,
            batches,
            state,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    enum Step {
        Data(Vec<u8>),
        WouldBlock,
        Interrupted,
        Eof,
    }

    struct ScriptedReader(VecDeque<Step>);

    impl Read for ScriptedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front().unwrap_or(Step::WouldBlock) {
                Step::Data(bytes) => {
                    assert!(bytes.len() <= buffer.len());
                    buffer[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Step::WouldBlock => Err(io::ErrorKind::WouldBlock.into()),
                Step::Interrupted => Err(io::ErrorKind::Interrupted.into()),
                Step::Eof => Ok(0),
            }
        }
    }

    #[test]
    fn only_eagain_marks_the_last_batch_as_drained() {
        let mut reader = ScriptedReader(VecDeque::from([
            Step::Data(b"one".to_vec()),
            Step::Interrupted,
            Step::Data(b"two".to_vec()),
            Step::WouldBlock,
        ]));
        let outcome = NonblockingBatchReader::new(8, 64)
            .read_cycle(&mut reader)
            .expect("scripted read should work");
        assert_eq!(outcome.state, ReadCycleState::DrainedToEagain);
        assert_eq!(outcome.batches.len(), 2);
        assert_eq!(outcome.batches[0].drain, DrainState::MoreInCurrentCycle);
        assert_eq!(outcome.batches[1].drain, DrainState::DrainedToEagain);
    }

    #[test]
    fn empty_eagain_still_emits_ordering_metadata() {
        let mut reader = ScriptedReader(VecDeque::from([Step::WouldBlock]));
        let outcome = NonblockingBatchReader::default()
            .read_cycle(&mut reader)
            .expect("scripted read should work");
        assert_eq!(outcome.batches.len(), 1);
        assert!(outcome.batches[0].bytes.is_empty());
        assert_eq!(outcome.batches[0].drain, DrainState::DrainedToEagain);
    }

    #[test]
    fn eof_and_budget_never_claim_convergence() {
        let mut eof = ScriptedReader(VecDeque::from([Step::Data(b"end".to_vec()), Step::Eof]));
        let outcome = NonblockingBatchReader::new(8, 64)
            .read_cycle(&mut eof)
            .expect("scripted read should work");
        assert_eq!(outcome.state, ReadCycleState::Eof);
        assert_eq!(outcome.batches[0].drain, DrainState::MoreInCurrentCycle);

        let mut busy = ScriptedReader(VecDeque::from([
            Step::Data(b"1234".to_vec()),
            Step::Data(b"5678".to_vec()),
        ]));
        let outcome = NonblockingBatchReader::new(4, 8)
            .read_cycle(&mut busy)
            .expect("scripted read should work");
        assert_eq!(outcome.state, ReadCycleState::BudgetExhausted);
        assert!(
            outcome
                .batches
                .iter()
                .all(|batch| batch.drain == DrainState::MoreInCurrentCycle)
        );
    }
}

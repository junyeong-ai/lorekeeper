//! Admission bounds for retained structured input, independent of process memory.

use std::io::Write;

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum InputError {
    #[error(
        "serialized input exceeds the {max_bytes}-byte admission budget; narrow the batch or raise ingest.inputs deliberately"
    )]
    Limit { max_bytes: usize },
    #[error("input cannot be serialized for byte admission")]
    Serialization,
}

/// Counts compact JSON values without allocating a serialized copy. Each
/// accumulation stage uses its own counter; this is not a process RSS ceiling.
#[derive(Clone, Debug)]
pub struct InputBudget {
    max_bytes: usize,
    bytes: usize,
}

impl InputBudget {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            bytes: 0,
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn remaining(&self) -> usize {
        self.max_bytes - self.bytes
    }

    pub fn admit(&mut self, value: &impl Serialize) -> Result<(), InputError> {
        struct Counter {
            remaining: usize,
            exceeded: bool,
        }
        impl Write for Counter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > self.remaining {
                    self.exceeded = true;
                    return Err(std::io::Error::other("input admission limit"));
                }
                self.remaining -= bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut counter = Counter {
            remaining: self.remaining(),
            exceeded: false,
        };
        serde_json::to_writer(&mut counter, value).map_err(|_| {
            if counter.exceeded {
                InputError::Limit {
                    max_bytes: self.max_bytes,
                }
            } else {
                InputError::Serialization
            }
        })?;
        self.bytes = self.max_bytes - counter.remaining;
        Ok(())
    }

    pub fn admit_all<T: Serialize>(&mut self, values: &[T]) -> Result<(), InputError> {
        for value in values {
            self.admit(value)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_admission_counts_json_escaping_and_refuses_without_consuming() {
        let first = "한국어\n\"quoted\"";
        let size = serde_json::to_vec(first).unwrap().len();
        let mut budget = InputBudget::new(size * 2);
        budget.admit(&first).unwrap();
        budget.admit(&first).unwrap();
        assert_eq!(budget.bytes(), size * 2);
        assert!(matches!(budget.admit(&""), Err(InputError::Limit { .. })));
        assert_eq!(budget.bytes(), size * 2);
        let mut smaller = InputBudget::new(size - 1);
        assert!(matches!(
            smaller.admit(&first),
            Err(InputError::Limit { .. })
        ));
        assert_eq!(smaller.bytes(), 0);
    }
}

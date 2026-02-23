use matchx_engine::MatchingEngine;
use matchx_journal::{AsyncJournal, AsyncJournalConfig, JournalError};
use matchx_types::{Command, InstrumentConfig, MatchEvent};
use std::path::Path;

pub struct EndToEndPipeline {
    engine: MatchingEngine,
    journal: AsyncJournal,
    input_seq: u64,
}

impl EndToEndPipeline {
    pub fn new(
        config: InstrumentConfig,
        arena_capacity: u32,
        journal_cfg: AsyncJournalConfig,
        path_prefix: impl AsRef<Path>,
    ) -> Result<Self, JournalError> {
        Ok(Self {
            engine: MatchingEngine::new(config, arena_capacity),
            journal: AsyncJournal::open(path_prefix, journal_cfg)?,
            input_seq: 0,
        })
    }

    pub fn process(&mut self, cmd: Command) -> Result<Vec<MatchEvent>, JournalError> {
        self.input_seq += 1;
        let events = self.engine.process(cmd.clone()).to_vec();
        self.journal.append(self.input_seq, &cmd)?;
        Ok(events)
    }

    pub fn accepted_sequence(&self) -> u64 {
        self.journal.accepted_sequence()
    }

    pub fn durable_sequence(&self) -> u64 {
        self.journal.durable_sequence()
    }
}

#[cfg(test)]
mod tests {
    use super::EndToEndPipeline;
    use matchx_journal::AsyncJournalConfig;
    use matchx_types::{Command, InstrumentConfig, OrderId, StpMode};

    #[test]
    fn pipeline_processes_command_and_enqueues_wal_record() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("journal");
        let mut p = EndToEndPipeline::new(test_config(), 1024, journal_cfg(), prefix).unwrap();
        let events = p.process(Command::CancelOrder { id: OrderId(42) }).unwrap();
        assert!(!events.is_empty());
        assert!(p.accepted_sequence() >= 1);
    }

    fn test_config() -> InstrumentConfig {
        InstrumentConfig {
            id: 1,
            tick_size: 1,
            lot_size: 1,
            base_price: 0,
            max_ticks: 10_000,
            stp_mode: StpMode::CancelNewest,
        }
    }

    fn journal_cfg() -> AsyncJournalConfig {
        AsyncJournalConfig::default()
    }
}

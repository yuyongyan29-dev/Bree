use super::{AppEvidence, RawProcess};
use crate::model::SystemMemory;

pub(crate) struct Backend;
impl Backend {
    pub fn new() -> Result<Self, String> {
        Err("This Alpha supports only macOS; the current system is unverified.".into())
    }
    pub fn current_uid(&self) -> u32 {
        0
    }
    pub fn system_memory(&self) -> SystemMemory {
        unreachable!("unsupported platform")
    }
    pub fn processes(&self) -> Result<(Vec<RawProcess>, Vec<String>), String> {
        unreachable!("unsupported platform")
    }
    pub fn applications(&self) -> Vec<AppEvidence> {
        vec![]
    }
}
pub(crate) fn pump_events() {}

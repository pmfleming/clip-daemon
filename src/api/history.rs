//! One observation barrier for token-sealed queries and invalidation baselines.
//! Content tokens are compared for equality, never sorted as delivery counters.
use crate::actions::ApiError;

#[derive(Default)]
pub(super) struct Clock {
    token: Option<u64>,
    revision: u64,
}
impl Clock {
    pub fn observe(&mut self, token: u64) -> Result<u64, ApiError> {
        if self.token != Some(token) {
            self.advance()?;
            self.token = Some(token);
        }
        Ok(self.revision)
    }
    pub fn invalidate(&mut self) {
        self.token = None;
    }
    fn advance(&mut self) -> Result<(), ApiError> {
        if self.revision >= 9_007_199_254_740_991 {
            return Err(ApiError::new(
                "history-unavailable",
                "History observation clock exhausted; restart required",
            ));
        }
        self.revision += 1;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokens_are_not_counters_and_outages_require_a_new_observation() {
        let mut clock = Clock::default();
        assert_eq!(clock.observe(100).unwrap(), 1);
        assert_eq!(clock.observe(2).unwrap(), 2);
        assert_eq!(clock.observe(2).unwrap(), 2);
        clock.invalidate();
        assert_eq!(clock.observe(2).unwrap(), 3);
        clock.revision = 9_007_199_254_740_991;
        assert!(clock.observe(3).is_err());
        assert_eq!(clock.token, Some(2));
    }
}

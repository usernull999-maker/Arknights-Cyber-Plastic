//! Runtime primitives shared by capture-adjacent business services.
//!
//! The recorder owns a `FrameTimeline`; UI consumers receive projections of
//! it and never participate in input-to-frame attribution.

use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservationStamp {
    pub sequence: u64,
    pub observed_at_ns: u64,
    pub elapsed_frames: i32,
    pub logical_frame: Option<i32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputStamp {
    pub observed_at_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameCorrelation {
    pub observation: ObservationStamp,
    /// Positive values mean the observation was available before the input.
    pub age_ns: i128,
}

/// Fixed-memory capture timeline. Future business services should retain raw
/// input timestamps and use this structure rather than consulting the GUI.
#[derive(Debug)]
pub struct FrameTimeline {
    capacity: usize,
    observations: VecDeque<ObservationStamp>,
}

impl FrameTimeline {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "frame timeline capacity must be non-zero");
        Self {
            capacity,
            observations: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, observation: ObservationStamp) {
        if self.observations.len() == self.capacity {
            self.observations.pop_front();
        }
        self.observations.push_back(observation);
    }

    pub fn correlate(&self, input: InputStamp) -> Option<FrameCorrelation> {
        let observation = self
            .observations
            .iter()
            .rev()
            .find(|observation| observation.observed_at_ns <= input.observed_at_ns)
            .or_else(|| self.observations.front())?;
        Some(FrameCorrelation {
            observation: *observation,
            age_ns: i128::from(input.observed_at_ns) - i128::from(observation.observed_at_ns),
        })
    }

    pub fn len(&self) -> usize {
        self.observations.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameTimeline, InputStamp, ObservationStamp};

    #[test]
    fn correlates_to_the_last_completed_observation() {
        let mut timeline = FrameTimeline::new(3);
        timeline.push(ObservationStamp {
            sequence: 1,
            observed_at_ns: 10,
            elapsed_frames: 1,
            logical_frame: Some(1),
        });
        timeline.push(ObservationStamp {
            sequence: 2,
            observed_at_ns: 70,
            elapsed_frames: 1,
            logical_frame: Some(1),
        });

        let correlation = timeline
            .correlate(InputStamp { observed_at_ns: 95 })
            .expect("timeline has observations");
        assert_eq!(correlation.observation.sequence, 2);
        assert_eq!(correlation.age_ns, 25);
    }

    #[test]
    fn retains_fixed_memory_during_long_or_paused_sessions() {
        let mut timeline = FrameTimeline::new(2);
        for sequence in 1..=3 {
            timeline.push(ObservationStamp {
                sequence,
                observed_at_ns: sequence * 1_000_000_000,
                elapsed_frames: 42,
                logical_frame: Some(42),
            });
        }

        assert_eq!(timeline.len(), 2);
        let correlation = timeline
            .correlate(InputStamp {
                observed_at_ns: 3_500_000_000,
            })
            .expect("timeline has observations");
        assert_eq!(correlation.observation.sequence, 3);
        assert_eq!(correlation.observation.elapsed_frames, 42);
    }
}

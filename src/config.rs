//! Configurable knobs for [`MemoryBudget`](crate::MemoryBudget).

use std::time::Duration;

/// Default cadence at which the budget reapportions cache caps.
pub(crate) const DEFAULT_TICK: Duration = Duration::from_secs(5);

/// Upper hysteresis band — RSS above `target * SHRINK_RATIO` triggers
/// a proportional shrink.
pub(crate) const SHRINK_RATIO: f64 = 1.05;

/// Lower hysteresis band — RSS below `target * GROW_RATIO` allows
/// caches to grow back toward their preferred ceiling.
pub(crate) const GROW_RATIO: f64 = 0.70;

/// Ceiling on the auto-derived target when no `MEMORY_BUDGET_TARGET_MIB`
/// is set: `min(0.8 * system_ram, 4096 MiB)`.
const AUTO_TARGET_CEILING_MIB: u64 = 4096;

/// Fraction of system RAM used when auto-deriving the target.
const AUTO_TARGET_FRACTION: f64 = 0.8;

/// Knobs that govern a [`MemoryBudget`](crate::MemoryBudget) instance.
///
/// Construct via [`BudgetConfig::from_env`] or build by hand for
/// tests. Field values are absolute bytes (not MiB) so downstream
/// code never has to second-guess the unit.
#[derive(Clone, Debug)]
pub struct BudgetConfig {
    /// Soft RSS target in bytes.
    target_bytes: u64,
    /// Optional hard ceiling (process aborts on overrun).
    hard_ceiling_bytes: Option<u64>,
    /// Reapportionment cadence.
    tick: Duration,
}

impl BudgetConfig {
    /// Build a config explicitly. Useful for tests.
    #[must_use]
    pub fn new(target_bytes: u64) -> Self {
        assert!(target_bytes > 0, "memory target must be greater than zero");
        Self {
            target_bytes,
            hard_ceiling_bytes: None,
            tick: DEFAULT_TICK,
        }
    }

    /// Resolve a config from environment variables.
    ///
    /// Reads:
    /// - `MEMORY_BUDGET_TARGET_MIB` (overrides the auto-derived target)
    /// - `MEMORY_BUDGET_TICK_SECS` (default `5`)
    /// - `MEMORY_BUDGET_HARD_CEILING_MIB` (optional)
    ///
    /// When `MEMORY_BUDGET_TARGET_MIB` is unset, the target is
    /// `min(0.8 × system_ram, 4096 MiB)`. `system_ram_bytes` is the
    /// installed RAM as seen by `sysinfo` and is required when the
    /// target is derived automatically.
    #[must_use]
    pub fn from_env(system_ram_bytes: Option<u64>) -> Self {
        let target_bytes = match read_mib_env("MEMORY_BUDGET_TARGET_MIB") {
            Some(mib) => mib.saturating_mul(MIB),
            None => auto_target_bytes(system_ram_bytes),
        };
        let hard_ceiling_bytes =
            read_mib_env("MEMORY_BUDGET_HARD_CEILING_MIB").map(|mib| mib.saturating_mul(MIB));
        let tick = read_secs_env("MEMORY_BUDGET_TICK_SECS")
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TICK);
        Self {
            target_bytes,
            hard_ceiling_bytes,
            tick,
        }
    }

    /// Soft RSS target in bytes.
    #[must_use]
    pub fn target_bytes(&self) -> u64 {
        self.target_bytes
    }

    /// Optional hard abort ceiling.
    #[must_use]
    pub fn hard_ceiling_bytes(&self) -> Option<u64> {
        self.hard_ceiling_bytes
    }

    /// Reapportionment cadence.
    #[must_use]
    pub fn tick(&self) -> Duration {
        self.tick
    }

    /// Override the reapportionment cadence. Convenience for tests
    /// that want to drive several ticks per second instead of one
    /// every five.
    #[must_use]
    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }
}

const MIB: u64 = 1024 * 1024;

fn auto_target_bytes(system_ram_bytes: Option<u64>) -> u64 {
    let ram = system_ram_bytes
        .expect("system RAM is required when MEMORY_BUDGET_TARGET_MIB is not configured");
    assert!(ram > 0, "system RAM must be greater than zero");
    let scaled = ((ram as f64) * AUTO_TARGET_FRACTION) as u64;
    let target = scaled.min(AUTO_TARGET_CEILING_MIB.saturating_mul(MIB));
    assert!(
        target > 0,
        "the derived memory target must be greater than zero"
    );
    target
}

fn read_mib_env(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    let mib = value
        .trim()
        .parse::<u64>()
        .unwrap_or_else(|error| panic!("{name} must be an unsigned integer: {error}"));
    assert!(mib > 0, "{name} must be greater than zero");
    Some(mib)
}

fn read_secs_env(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    let seconds = value
        .trim()
        .parse::<u64>()
        .unwrap_or_else(|error| panic!("{name} must be an unsigned integer: {error}"));
    assert!(seconds > 0, "{name} must be greater than zero");
    Some(seconds)
}

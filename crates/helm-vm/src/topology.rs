//! The CPU topology of a hybrid part.
//!
//! The running kernel does not publish `topology/core_type`, so the split
//! between performance and efficiency cores is taken from the two cpulists the
//! hybrid scheduler exposes instead. Everything here only reads the sysfs, so
//! the parsing can be covered by ordinary unit tests.

/// The CPUs of a machine, split by the role the scheduler gives them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CpuTopology {
    /// Performance cores, handed to the guest while a game runs.
    pub performance: Vec<u32>,
    /// Efficiency cores, kept by the host so the desktop stays responsive.
    pub efficiency: Vec<u32>,
}

impl CpuTopology {
    /// Read the topology of the running machine.
    pub fn read() -> Self {
        Self::from_lists(
            read_cpu_list("/sys/devices/cpu_core/cpus"),
            read_cpu_list("/sys/devices/cpu_atom/cpus"),
            read_cpu_list("/sys/devices/system/cpu/online"),
        )
    }

    /// Build a topology from the three cpulists.
    ///
    /// A machine that is not hybrid publishes neither `cpu_core` nor `cpu_atom`.
    /// Every online CPU is then treated as a performance core, so the plan keeps
    /// working instead of coming out empty.
    pub fn from_lists(
        performance: Option<Vec<u32>>,
        efficiency: Option<Vec<u32>>,
        online: Option<Vec<u32>>,
    ) -> Self {
        match performance {
            Some(performance) if !performance.is_empty() => {
                let mut efficiency = efficiency.unwrap_or_default();
                efficiency.retain(|cpu| !performance.contains(cpu));
                Self {
                    performance,
                    efficiency,
                }
            }
            _ => Self {
                performance: online.unwrap_or_default(),
                efficiency: Vec::new(),
            },
        }
    }

    /// True when the split between the two kinds of core is real.
    pub fn is_hybrid(&self) -> bool {
        !self.performance.is_empty() && !self.efficiency.is_empty()
    }

    /// Number of hardware threads the guest should get.
    ///
    /// Only the performance cores are handed over: the efficiency cores are
    /// what keeps the host usable while the game runs, which is the whole point
    /// of the split.
    pub fn guest_threads(&self) -> usize {
        self.performance.len()
    }

    /// `taskset` mask for the guest: the performance cores, comma separated.
    pub fn guest_affinity(&self) -> String {
        join_ranges(&self.performance)
    }

    /// `taskset` mask for everything the host keeps.
    pub fn host_affinity(&self) -> String {
        join_ranges(&self.efficiency)
    }
}

/// Read a kernel cpulist (`0-11`, `0,2-3,7`) into the CPUs it names.
fn read_cpu_list(path: &str) -> Option<Vec<u32>> {
    let raw = std::fs::read_to_string(path).ok()?;
    parse_cpu_list(raw.trim()).ok()
}

/// Parse one kernel cpulist.
pub fn parse_cpu_list(raw: &str) -> Result<Vec<u32>, String> {
    let mut cpus = Vec::new();
    for item in raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        match item.split_once('-') {
            Some((first, last)) => {
                let first: u32 = first
                    .trim()
                    .parse()
                    .map_err(|_| format!("not a cpu range: {item}"))?;
                let last: u32 = last
                    .trim()
                    .parse()
                    .map_err(|_| format!("not a cpu range: {item}"))?;
                if last < first {
                    return Err(format!("descending cpu range: {item}"));
                }
                cpus.extend(first..=last);
            }
            None => cpus.push(
                item.parse()
                    .map_err(|_| format!("not a cpu number: {item}"))?,
            ),
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    Ok(cpus)
}

/// Render a CPU set back into a cpulist, collapsing runs into ranges.
fn join_ranges(cpus: &[u32]) -> String {
    let mut sorted = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut parts: Vec<String> = Vec::new();
    let mut index = 0;
    while index < sorted.len() {
        let start = sorted[index];
        let mut end = start;
        while index + 1 < sorted.len() && sorted[index + 1] == end + 1 {
            index += 1;
            end = sorted[index];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        index += 1;
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_is_expanded() {
        assert_eq!(parse_cpu_list("0-3").unwrap(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_mixed_list_is_sorted_and_deduplicated() {
        assert_eq!(parse_cpu_list("12-13,0,1,1").unwrap(), vec![0, 1, 12, 13]);
    }

    #[test]
    fn a_broken_list_is_an_error() {
        assert!(parse_cpu_list("a-b").is_err());
        assert!(parse_cpu_list("5-2").is_err());
    }

    #[test]
    fn ranges_are_collapsed() {
        assert_eq!(join_ranges(&[0, 1, 2, 3, 12, 13]), "0-3,12-13");
        assert_eq!(join_ranges(&[7]), "7");
    }

    #[test]
    fn a_hybrid_split_keeps_the_two_kinds_apart() {
        let topology = CpuTopology::from_lists(
            Some((0..=11).collect()),
            Some((12..=19).collect()),
            Some((0..=19).collect()),
        );
        assert!(topology.is_hybrid());
        assert_eq!(topology.guest_threads(), 12);
        assert_eq!(topology.guest_affinity(), "0-11");
        assert_eq!(topology.host_affinity(), "12-19");
    }

    #[test]
    fn a_non_hybrid_machine_offers_every_cpu_to_the_guest() {
        let topology = CpuTopology::from_lists(None, None, Some(vec![0, 1, 2, 3]));
        assert!(!topology.is_hybrid());
        assert_eq!(topology.guest_affinity(), "0-3");
        assert!(topology.host_affinity().is_empty());
    }

    #[test]
    fn an_efficiency_core_can_never_also_be_a_performance_core() {
        let topology = CpuTopology::from_lists(Some(vec![0, 1]), Some(vec![1, 2]), None);
        assert_eq!(topology.efficiency, vec![2]);
    }
}

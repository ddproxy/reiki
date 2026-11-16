//! Pure model for the matrix view: peers x providers, no drawing. A peer that does not use a
//! provider is `Absent`, never a failure, because peers publish different provider sets.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{Fleet, Health, Upstream};

/// One peer/provider cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    /// The peer has no such upstream, or the upstream has zero keys of any state.
    Absent,
    /// Every key is available.
    Healthy,
    /// Some keys healthy, the rest cooling or disabled; `cooling` is that non-healthy count, so
    /// healthy keys = `total - cooling`.
    Partial {
        cooling: u32,
        total: u32,
        soonest: Option<Duration>,
    },
    /// No healthy key, at least one cooling; `class` is the soonest cooling key's class, if the
    /// peer publishes labels.
    Cooling {
        soonest: Option<Duration>,
        class: Option<String>,
    },
    /// No healthy and no cooling key; every key is out of rotation.
    Disabled,
    /// The peer is stale or unreachable; every cell of that row.
    Stale,
}

/// One provider column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub upstream: String,
    /// Peers that publish this upstream, fresh or stale.
    pub peers_with: usize,
    /// Of those, peers whose cell is `Healthy`.
    pub healthy_peers: usize,
}

impl Column {
    /// `"<healthy>/<with>"`, the one number a cell grid needs.
    pub fn total_label(&self) -> String {
        format!("{}/{}", self.healthy_peers, self.peers_with)
    }
}

/// Host figures, all `None` when absent or malformed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HostSummary {
    pub load1: Option<f64>,
    pub disk_percent: Option<f64>,
    pub sessions: Option<u64>,
}

impl HostSummary {
    /// Reads `load_average[0]`, the worst disk `used_percent` and `active_sessions`.
    pub fn from_health(health: &Health) -> Self {
        let host = health.host.as_ref();
        Self {
            load1: host
                .and_then(|h| h.get("load_average"))
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0),
            disk_percent: host
                .and_then(|h| h.get("disks"))
                .and_then(Value::as_array)
                .and_then(|disks| {
                    disks
                        .iter()
                        .filter_map(|d| d.get("used_percent"))
                        .filter_map(Value::as_f64)
                        .filter(|p| p.is_finite())
                        .map(|p| p.clamp(0.0, 100.0))
                        .reduce(f64::max)
                }),
            sessions: health
                .sessions
                .as_ref()
                .and_then(|s| s.get("active_sessions"))
                .and_then(Value::as_u64),
        }
    }
}

/// One peer row: a cell per column, in column order.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub peer: String,
    pub cells: Vec<Cell>,
    /// Cooling combinations per column, in column order; 0 where the peer has no such upstream
    /// or its data is stale.
    pub combos: Vec<u32>,
    pub host: HostSummary,
    pub error: Option<String>,
    pub stale: bool,
    /// How long ago this host received the peer's data; `None` when it never did.
    pub age: Option<Duration>,
}

/// The matrix itself. `build` keeps the caller's peer order in `rows`;
/// [`order_peers`] returns the display order as indices into `rows`.
#[derive(Debug, Clone, PartialEq)]
pub struct Matrix {
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    /// Providers left out because a peer published more than [`MAX_COLUMNS`] distinct names.
    pub hidden_columns: usize,
}

/// The most provider columns kept. Peers choose their upstream names, so an unbounded set
/// would let one peer widen every row.
pub const MAX_COLUMNS: usize = 32;

impl Matrix {
    pub fn build(fleet: &Fleet, peers: &[String], now: Instant) -> Matrix {
        // One pass for the column union: a peer with data counts towards peers_with once, even
        // if it repeats an upstream name.
        let mut with: HashMap<&str, usize> = HashMap::new();
        for name in peers {
            let Some(view) = fleet.peer(name, now) else {
                continue;
            };
            let mut seen = HashSet::new();
            for upstream in &view.health.upstreams {
                if seen.insert(upstream.name.as_str()) {
                    *with.entry(upstream.name.as_str()).or_insert(0) += 1;
                }
            }
        }
        let mut columns: Vec<Column> = with
            .into_iter()
            .map(|(upstream, peers_with)| Column {
                upstream: upstream.to_owned(),
                peers_with,
                healthy_peers: 0,
            })
            .collect();
        columns.sort_by(|a, b| {
            b.peers_with
                .cmp(&a.peers_with)
                .then_with(|| a.upstream.cmp(&b.upstream))
        });
        let hidden_columns = columns.len().saturating_sub(MAX_COLUMNS);
        columns.truncate(MAX_COLUMNS);

        let mut rows = Vec::with_capacity(peers.len());
        for name in peers {
            let view = fleet.peer(name, now);
            let fresh = view.as_ref().filter(|v| !v.stale);
            let upstream = |col: &Column| {
                fresh.and_then(|v| v.health.upstreams.iter().find(|u| u.name == col.upstream))
            };
            let has_upstream = |col: &Column| {
                view.as_ref()
                    .is_some_and(|v| v.health.upstreams.iter().any(|u| u.name == col.upstream))
            };
            let host = fresh
                .map(|v| HostSummary::from_health(v.health))
                .unwrap_or_default();
            rows.push(Row {
                peer: name.clone(),
                cells: columns
                    .iter()
                    .map(|col| match fresh {
                        Some(_) => upstream(col).map_or(Cell::Absent, cell_of),
                        None if view.is_some() && !has_upstream(col) => Cell::Absent,
                        None => Cell::Stale,
                    })
                    .collect(),
                combos: columns
                    .iter()
                    .map(|col| upstream(col).map_or(0, |u| u.cooling_combinations))
                    .collect(),
                host,
                error: fleet.error(name).map(str::to_owned),
                stale: fresh.is_none(),
                age: view.as_ref().map(|v| v.age),
            });
        }
        for (i, col) in columns.iter_mut().enumerate() {
            col.healthy_peers = rows
                .iter()
                .filter(|r| r.cells.get(i) == Some(&Cell::Healthy))
                .count();
        }
        Matrix {
            columns,
            rows,
            hidden_columns,
        }
    }

    /// Returns a view with only `provider` and only peers whose cell for it is not `Absent`.
    /// `None` when no such provider column exists.
    pub fn filter_provider(&self, provider: &str) -> Option<Matrix> {
        let idx = self.columns.iter().position(|c| c.upstream == provider)?;
        let rows: Vec<Row> = self
            .rows
            .iter()
            .filter(|r| r.cells.get(idx) != Some(&Cell::Absent))
            .map(|r| Row {
                peer: r.peer.clone(),
                cells: vec![r.cells[idx].clone()],
                combos: vec![r.combos.get(idx).copied().unwrap_or(0)],
                host: r.host.clone(),
                error: r.error.clone(),
                stale: r.stale,
                age: r.age,
            })
            .collect();
        // keep original order (same as build input order) which is fine; display order applied later
        let healthy_peers = rows
            .iter()
            .filter(|r| r.cells.first() == Some(&Cell::Healthy))
            .count();
        let column = Column {
            upstream: self.columns[idx].upstream.clone(),
            peers_with: rows.len(),
            healthy_peers,
        };
        Some(Matrix {
            columns: vec![column],
            rows,
            hidden_columns: 0,
        })
    }
}

/// Display order: problem peers (stale, or any `Partial`/`Cooling`/`Disabled` cell) first, then
/// by name. A peer with only `Absent` and `Healthy` cells is not a problem.
pub fn order_peers(matrix: &Matrix) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..matrix.rows.len()).collect();
    idx.sort_by(|&a, &b| {
        let (ra, rb) = (&matrix.rows[a], &matrix.rows[b]);
        is_problem(rb)
            .cmp(&is_problem(ra))
            .then_with(|| ra.peer.cmp(&rb.peer))
    });
    idx
}

/// Maps a display position to a position in `0..len`, wrapping, so a selection left over from a
/// longer list still points at a row. `None` when there are no rows.
pub fn resolve(selected: Option<usize>, len: usize) -> Option<usize> {
    (len > 0).then(|| selected.unwrap_or(0) % len)
}

/// Whether a row needs attention: stale, or any cell that is not fully healthy or absent.
pub fn is_problem(row: &Row) -> bool {
    row.stale
        || row.cells.iter().any(|c| {
            matches!(
                c,
                Cell::Partial { .. } | Cell::Cooling { .. } | Cell::Disabled
            )
        })
}

/// One host-metrics sample, as pushed by the caller.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Sample {
    pub load1: Option<f64>,
    pub disk_percent: Option<f64>,
    pub sessions: Option<u64>,
}

/// Which figure of a [`Sample`] a series covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Load1,
    DiskPercent,
    Sessions,
}

/// Per-peer ring buffer of samples. Bounded: `capacity` samples per peer, at most 64 peers
/// (least recently pushed dropped first).
#[derive(Clone)]
pub struct History {
    capacity: usize,
    seq: u64,
    peers: HashMap<String, Ring>,
}

#[derive(Clone)]
struct Ring {
    samples: VecDeque<Sample>,
    last_push: u64,
}

impl History {
    /// Retained samples per peer.
    pub const DEFAULT_CAPACITY: usize = 60;
    /// Distinct peers tracked.
    pub const MAX_PEERS: usize = 64;
    /// Load/sessions scale floor, so an idle host draws flat instead of full-scale.
    const SCALE_FLOOR: f64 = 1.0;

    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            seq: 0,
            peers: HashMap::new(),
        }
    }

    pub fn push(&mut self, peer: &str, sample: Sample) {
        if self.capacity == 0 {
            return;
        }
        self.seq += 1;
        if let Some(ring) = self.peers.get_mut(peer) {
            if ring.samples.len() >= self.capacity {
                ring.samples.pop_front();
            }
            ring.samples.push_back(sample);
            ring.last_push = self.seq;
            return;
        }
        if self.peers.len() >= Self::MAX_PEERS {
            let victim = self
                .peers
                .iter()
                .min_by_key(|(_, ring)| ring.last_push)
                .map(|(k, _)| k.clone());
            if let Some(victim) = victim {
                self.peers.remove(&victim);
            }
        }
        let mut samples = VecDeque::with_capacity(self.capacity);
        samples.push_back(sample);
        self.peers.insert(
            peer.to_owned(),
            Ring {
                samples,
                last_push: self.seq,
            },
        );
    }

    /// Levels 0..=8 over the retained window. Samples missing the metric are skipped, so the
    /// series can be shorter than the window. Scaling is stated on [`Metric`].
    pub fn series(&self, peer: &str, metric: Metric) -> Vec<u64> {
        let Some(ring) = self.peers.get(peer) else {
            return Vec::new();
        };
        let values: Vec<f64> = ring
            .samples
            .iter()
            .filter_map(|s| value(s, metric))
            .collect();
        if values.is_empty() {
            return Vec::new();
        }
        // Load and sessions scale to the window maximum, floored at 1.0 so an idle host draws
        // low and flat instead of full-scale; disk is already 0..100 absolute.
        let scale = match metric {
            Metric::DiskPercent => 100.0,
            _ => {
                let max = values.iter().fold(0.0_f64, |m, &v| m.max(v));
                max.max(Self::SCALE_FLOOR)
            }
        };
        values
            .iter()
            .map(|&v| ((v / scale) * 8.0).round().clamp(0.0, 8.0) as u64)
            .collect()
    }
}

fn cell_of(upstream: &Upstream) -> Cell {
    let total = upstream
        .healthy_keys
        .saturating_add(upstream.cooling_keys)
        .saturating_add(upstream.disabled_keys);
    if total == 0 {
        return Cell::Absent;
    }
    let soonest = upstream.soonest_cooldown_seconds.map(Duration::from_secs);
    if upstream.healthy_keys > 0 {
        let down = upstream.cooling_keys.saturating_add(upstream.disabled_keys);
        return if down == 0 {
            Cell::Healthy
        } else {
            Cell::Partial {
                cooling: down,
                total,
                soonest,
            }
        };
    }
    if upstream.cooling_keys > 0 {
        return Cell::Cooling {
            soonest,
            class: cooldown_class(upstream),
        };
    }
    Cell::Disabled
}

/// The class of the labelled cooling key with the soonest cooldown, when labels are published.
pub fn cooldown_class(upstream: &Upstream) -> Option<String> {
    upstream
        .keys
        .as_ref()?
        .iter()
        .filter(|k| k.state == "cooling")
        .min_by_key(|k| k.cooldown_seconds.unwrap_or(u64::MAX))
        .and_then(|k| k.cooldown_class.clone())
}

fn value(sample: &Sample, metric: Metric) -> Option<f64> {
    match metric {
        Metric::Load1 => sample.load1.filter(|v| v.is_finite() && *v >= 0.0),
        Metric::DiskPercent => sample
            .disk_percent
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, 100.0)),
        Metric::Sessions => sample.sessions.map(|n| n as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_AGE: Duration = Duration::from_secs(90);

    fn fleet() -> Fleet {
        Fleet::new(MAX_AGE)
    }

    fn add(fleet: &mut Fleet, name: &str, doc: &str, at: Instant) {
        fleet.apply_snapshot(name, Health::from_json(doc).unwrap(), at);
    }

    fn upstream(name: &str, healthy: u32, cooling: u32, disabled: u32) -> String {
        format!(
            r#"{{"name":"{name}","healthy_keys":{healthy},"cooling_keys":{cooling},"disabled_keys":{disabled},"soonest_cooldown_seconds":null,"last_status_class":null}}"#
        )
    }

    fn doc(upstreams: &str) -> String {
        format!(r#"{{"instance":"x","observed_at":"t","upstreams":[{upstreams}]}}"#)
    }

    fn names(peers: &[&str]) -> Vec<String> {
        peers.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn mismatched_providers_union_columns_and_absent_cells() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &doc(&upstream("zen", 2, 0, 0)), t0);
        add(
            &mut f,
            "peer-b",
            &doc(&format!(
                "{},{}",
                upstream("openrouter", 1, 0, 0),
                upstream("gemini", 0, 2, 0)
            )),
            t0,
        );
        add(
            &mut f,
            "peer-c",
            &doc(&format!(
                "{},{}",
                upstream("zen", 1, 0, 0),
                upstream("gemini", 3, 0, 0)
            )),
            t0,
        );
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b", "peer-c"]), t0);
        assert_eq!(
            m.columns
                .iter()
                .map(|c| c.upstream.as_str())
                .collect::<Vec<_>>(),
            ["gemini", "zen", "openrouter"],
            "peers_with 2 first (name asc within), then 1"
        );
        assert_eq!(
            m.columns
                .iter()
                .map(|c| (c.peers_with, c.healthy_peers))
                .collect::<Vec<_>>(),
            [(2, 1), (2, 2), (1, 1)]
        );
        assert_eq!(
            m.rows[0].cells,
            vec![Cell::Absent, Cell::Healthy, Cell::Absent]
        );
        assert_eq!(
            m.rows[1].cells,
            vec![
                Cell::Cooling {
                    soonest: None,
                    class: None
                },
                Cell::Absent,
                Cell::Healthy
            ]
        );
        assert_eq!(
            m.rows[2].cells,
            vec![Cell::Healthy, Cell::Healthy, Cell::Absent]
        );
    }

    #[test]
    fn partial_cooling_and_disabled_are_distinct_cells() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &doc(&upstream("zen", 2, 1, 1)), t0);
        add(&mut f, "peer-b", &doc(&upstream("zen", 0, 1, 0)), t0);
        add(&mut f, "peer-c", &doc(&upstream("zen", 0, 0, 3)), t0);
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b", "peer-c"]), t0);
        assert_eq!(
            m.rows[0].cells[0],
            Cell::Partial {
                cooling: 2,
                total: 4,
                soonest: None
            }
        );
        assert_eq!(
            m.rows[1].cells[0],
            Cell::Cooling {
                soonest: None,
                class: None
            }
        );
        assert_eq!(m.rows[2].cells[0], Cell::Disabled);
    }

    #[test]
    fn partial_and_cooling_carry_soonest_and_class_from_labels() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &doc(
                r#"{"name":"zen","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,
                 "soonest_cooldown_seconds":300,"last_status_class":"4xx",
                 "keys":[{"name":"b","state":"cooling","cooldown_seconds":300,
                          "cooldown_class":"quota_exhausted"}]}"#,
            ),
            t0,
        );
        add(
            &mut f,
            "peer-b",
            &doc(
                r#"{"name":"zen","healthy_keys":0,"cooling_keys":2,"disabled_keys":0,
                 "soonest_cooldown_seconds":42,"last_status_class":"4xx",
                 "keys":[{"name":"a","state":"cooling","cooldown_seconds":300,
                          "cooldown_class":"rate_limited"},
                         {"name":"b","state":"cooling","cooldown_seconds":42,
                          "cooldown_class":"quota_exhausted"}]}"#,
            ),
            t0,
        );
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b"]), t0);
        assert_eq!(
            m.rows[0].cells[0],
            Cell::Partial {
                cooling: 1,
                total: 2,
                soonest: Some(Duration::from_secs(300))
            }
        );
        assert_eq!(
            m.rows[1].cells[0],
            Cell::Cooling {
                soonest: Some(Duration::from_secs(42)),
                class: Some("quota_exhausted".to_owned()),
            },
            "the soonest cooling key carries the class"
        );
    }

    #[test]
    fn a_zero_key_upstream_is_absent() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &doc(&upstream("zen", 0, 0, 0)), t0);
        let m = Matrix::build(&f, &names(&["peer-a"]), t0);
        assert_eq!(m.rows[0].cells, vec![Cell::Absent]);
    }

    #[test]
    fn stale_and_unseen_peers_are_all_stale_and_still_appear() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &doc(&upstream("zen", 2, 0, 0)), t0);
        add(
            &mut f,
            "peer-b",
            &doc(&upstream("zen", 1, 0, 0)),
            t0 - Duration::from_secs(91),
        );
        f.set_error("peer-c", "poll: cannot connect");
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b", "peer-c"]), t0);
        assert_eq!(m.rows[0].cells, vec![Cell::Healthy]);
        assert_eq!(
            m.rows[1].cells,
            vec![Cell::Stale],
            "stale never reads healthy"
        );
        assert!(m.rows[1].stale);
        assert_eq!(m.rows[2].cells, vec![Cell::Stale], "never-seen too");
        assert_eq!(m.rows[2].host, HostSummary::default());
        assert_eq!(m.rows[2].error.as_deref(), Some("poll: cannot connect"));
        assert_eq!(m.rows.len(), 3);
        assert_eq!(
            m.columns[0].peers_with, 2,
            "stale data still names the column"
        );
        assert_eq!(m.columns[0].healthy_peers, 1);
    }

    #[test]
    fn problem_peers_sort_first_then_by_name() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &doc(&upstream("zen", 1, 0, 0)), t0);
        add(&mut f, "peer-b", &doc(&upstream("zen", 0, 0, 2)), t0);
        add(&mut f, "peer-c", &doc(&upstream("zen", 1, 1, 0)), t0);
        add(
            &mut f,
            "peer-d",
            &doc(&upstream("zen", 2, 0, 0)),
            t0 - Duration::from_secs(95),
        );
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b", "peer-c", "peer-d"]), t0);
        let order = order_peers(&m);
        assert_eq!(
            order
                .iter()
                .map(|&i| m.rows[i].peer.as_str())
                .collect::<Vec<_>>(),
            ["peer-b", "peer-c", "peer-d", "peer-a"],
            "every problem peer first, then name; stale is a problem"
        );
    }

    #[test]
    fn counts_only_cover_peers_that_have_the_provider() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &doc(&upstream("zen", 2, 0, 0)), t0);
        add(&mut f, "peer-b", &doc(&upstream("gem", 0, 1, 0)), t0);
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b"]), t0);
        let zen = m.columns.iter().find(|c| c.upstream == "zen").unwrap();
        assert_eq!((zen.peers_with, zen.healthy_peers), (1, 1));
        assert_eq!(zen.total_label(), "1/1");
        let gem = m.columns.iter().find(|c| c.upstream == "gem").unwrap();
        assert_eq!((gem.peers_with, gem.healthy_peers), (1, 0));
        assert_eq!(gem.total_label(), "0/1");
    }

    #[test]
    fn host_summary_takes_worst_disk_and_survives_hostile_json() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"x","observed_at":"t",
               "upstreams":[],
               "host":{"load_average":[0.5,1.5],"cores":8,
                       "disks":[{"used_percent":40.0},{"used_percent":90.5},{"used_percent":10.0}]},
               "sessions":{"active_sessions":3}}"#,
            t0,
        );
        add(
            &mut f,
            "peer-b",
            r#"{"instance":"x","observed_at":"t","upstreams":[],
               "host":"not an object",
               "sessions":{"active_sessions":"three"}}"#,
            t0,
        );
        add(
            &mut f,
            "peer-c",
            r#"{"instance":"x","observed_at":"t","upstreams":[],
               "host":{"load_average":[null,-1.0,1e308],"disks":[{"used_percent":"high"},
                       {"used_percent":150.0},{"used_percent":-5.0}]},
               "sessions":{"active_sessions":1e30}}"#,
            t0,
        );
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b", "peer-c"]), t0);
        assert_eq!(
            m.rows[0].host,
            HostSummary {
                load1: Some(0.5),
                disk_percent: Some(90.5),
                sessions: Some(3)
            }
        );
        assert_eq!(m.rows[1].host, HostSummary::default());
        assert_eq!(
            m.rows[2].host,
            HostSummary {
                load1: None,
                disk_percent: Some(100.0),
                sessions: None
            }
        );
    }

    #[test]
    fn history_bounds_samples_and_peers() {
        let mut h = History::new(3);
        for i in 0..5 {
            h.push(
                "peer-a",
                Sample {
                    sessions: Some(i),
                    ..Default::default()
                },
            );
        }
        assert_eq!(h.series("peer-a", Metric::Sessions), vec![4, 6, 8]);
        for i in 0..History::MAX_PEERS {
            h.push(&format!("peer-{i:02}"), Sample::default());
        }
        h.push(
            "peer-64",
            Sample {
                sessions: Some(7),
                ..Default::default()
            },
        );
        assert_eq!(h.peers.len(), History::MAX_PEERS, "at most 64 peers");
        assert!(
            h.series("peer-00", Metric::Sessions).is_empty(),
            "least recently pushed dropped"
        );
        assert_eq!(h.series("peer-64", Metric::Sessions), vec![8]);
    }

    #[test]
    fn history_series_scale_to_nine_levels() {
        let mut h = History::new(60);
        for load in [0.0_f64, 0.5, 1.0] {
            h.push(
                "peer-a",
                Sample {
                    load1: Some(load),
                    ..Default::default()
                },
            );
        }
        assert_eq!(h.series("peer-a", Metric::Load1), vec![0, 4, 8]);
        let mut h = History::new(60);
        for load in [0.0_f64, 0.0] {
            h.push(
                "peer-a",
                Sample {
                    load1: Some(load),
                    ..Default::default()
                },
            );
        }
        assert_eq!(
            h.series("peer-a", Metric::Load1),
            vec![0, 0],
            "idle host is flat"
        );
        let mut h = History::new(60);
        for pct in [50.0_f64, 100.0] {
            h.push(
                "peer-a",
                Sample {
                    disk_percent: Some(pct),
                    ..Default::default()
                },
            );
        }
        assert_eq!(
            h.series("peer-a", Metric::DiskPercent),
            vec![4, 8],
            "disk is 0..100 absolute"
        );
        let mut h = History::new(60);
        for n in [0_u64, 5, 10] {
            h.push(
                "peer-a",
                Sample {
                    sessions: Some(n),
                    ..Default::default()
                },
            );
        }
        assert_eq!(h.series("peer-a", Metric::Sessions), vec![0, 4, 8]);
        assert!(h.series("nobody", Metric::Load1).is_empty());
        let mut h = History::new(60);
        h.push(
            "peer-a",
            Sample {
                load1: Some(f64::NAN),
                ..Default::default()
            },
        );
        h.push(
            "peer-a",
            Sample {
                load1: Some(-1.0),
                ..Default::default()
            },
        );
        assert!(
            h.series("peer-a", Metric::Load1).is_empty(),
            "hostile samples are skipped"
        );
    }

    #[test]
    fn same_input_builds_the_same_matrix() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &doc(&upstream("zen", 1, 1, 0)), t0);
        add(&mut f, "peer-b", &doc(&upstream("gem", 0, 0, 1)), t0);
        let peers = names(&["peer-a", "peer-b"]);
        let a = Matrix::build(&f, &peers, t0);
        let b = Matrix::build(&f, &peers, t0);
        assert_eq!(a, b);
        assert_eq!(order_peers(&a), order_peers(&b));
    }

    #[test]
    fn cooling_combinations_are_carried_per_column_and_zero_when_absent() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &doc(
                r#"{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                    "soonest_cooldown_seconds":null,"last_status_class":null,
                    "cooling_combinations":2}"#,
            ),
            t0,
        );
        add(
            &mut f,
            "peer-b",
            &doc(
                r#"{"name":"gem","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                    "soonest_cooldown_seconds":null,"last_status_class":null,
                    "cooling_combinations":5}"#,
            ),
            t0,
        );
        add(
            &mut f,
            "peer-c",
            &doc(
                r#"{"name":"zen","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                    "soonest_cooldown_seconds":null,"last_status_class":null,
                    "cooling_combinations":9}"#,
            ),
            t0 - Duration::from_secs(91),
        );
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b", "peer-c"]), t0);
        assert_eq!(
            m.columns
                .iter()
                .map(|c| c.upstream.as_str())
                .collect::<Vec<_>>(),
            ["zen", "gem"],
            "the stale peer still names zen, so it has two peers"
        );
        assert_eq!(m.rows[0].combos, vec![2, 0]);
        assert_eq!(m.rows[1].combos, vec![0, 5], "absent is zero");
        assert_eq!(m.rows[2].combos, vec![0, 0], "stale data counts nothing");
    }

    #[test]
    fn rows_carry_their_age_and_never_seen_peers_have_none() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &doc(&upstream("zen", 1, 0, 0)), t0);
        add(
            &mut f,
            "peer-b",
            &doc(&upstream("zen", 1, 0, 0)),
            t0 - Duration::from_secs(91),
        );
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b", "peer-c"]), t0);
        assert_eq!(m.rows[0].age, Some(Duration::ZERO));
        assert_eq!(m.rows[1].age, Some(Duration::from_secs(91)));
        assert_eq!(m.rows[2].age, None);
    }

    #[test]
    fn resolve_wraps_and_has_no_rows_for_an_empty_list() {
        assert_eq!(resolve(None, 3), Some(0));
        assert_eq!(resolve(Some(1), 3), Some(1));
        assert_eq!(resolve(Some(4), 3), Some(1), "wraps past the end");
        assert_eq!(resolve(Some(usize::MAX), 3), Some(usize::MAX % 3));
        assert_eq!(resolve(Some(0), 0), None);
        assert_eq!(resolve(None, 0), None);
    }

    #[test]
    fn hostile_input_never_panics() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"x","observed_at":"t","upstreams":[
                {"name":"zen","healthy_keys":0,"cooling_keys":0,"disabled_keys":0,
                 "soonest_cooldown_seconds":18446744073709551615,"last_status_class":null,
                 "keys":[{"name":"a","state":"cooling","cooldown_seconds":null,"cooldown_class":null},
                         {"name":"b","state":"weird","cooldown_seconds":5,"cooldown_class":null}]}]}"#,
            t0,
        );
        add(&mut f, "peer-a", &doc(&upstream("zen", 1, 1, 1)), t0);
        add(
            &mut f,
            "peer-a",
            r#"{"instance":"x","observed_at":"t","upstreams":[{"name":"zen","healthy_keys":1,"cooling_keys":1,"disabled_keys":0,"soonest_cooldown_seconds":null,"last_status_class":null},{"name":"zen","healthy_keys":0,"cooling_keys":5,"disabled_keys":0,"soonest_cooldown_seconds":7,"last_status_class":null}]}"#,
            t0,
        );
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b"]), t0);
        assert_eq!(m.rows.len(), 2);
        assert_eq!(m.columns.len(), 1, "a duplicated upstream is one column");
        assert_eq!(m.columns[0].peers_with, 1);
        assert_eq!(
            m.rows[0].cells[0],
            Cell::Partial {
                cooling: 1,
                total: 2,
                soonest: None
            },
            "first duplicate wins"
        );
        assert_eq!(order_peers(&m).len(), 2);
        let mut h = History::new(0);
        h.push("peer-a", Sample::default());
        assert!(h.series("peer-a", Metric::Load1).is_empty());
    }

    #[test]
    fn columns_are_capped_and_the_hidden_count_is_reported() {
        let mut f = fleet();
        let t0 = Instant::now();
        let many: Vec<String> = (0..40)
            .map(|i| upstream(&format!("up-{i:02}"), 1, 0, 0))
            .collect();
        add(&mut f, "peer-a", &doc(&many.join(",")), t0);
        let m = Matrix::build(&f, &names(&["peer-a"]), t0);
        assert_eq!(m.columns.len(), MAX_COLUMNS);
        assert_eq!(m.hidden_columns, 40 - MAX_COLUMNS);
        assert_eq!(m.rows[0].cells.len(), MAX_COLUMNS);
        assert_eq!(m.rows[0].combos.len(), MAX_COLUMNS);
    }

    #[test]
    fn filter_keeps_one_provider_and_hides_absent_peers() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &doc(&upstream("zen", 2, 0, 0)), t0);
        add(
            &mut f,
            "peer-b",
            &doc(&format!(
                "{},{}",
                upstream("openrouter", 1, 0, 0),
                upstream("gemini", 0, 2, 0)
            )),
            t0,
        );
        add(
            &mut f,
            "peer-c",
            &doc(&format!(
                "{},{}",
                upstream("zen", 1, 0, 0),
                upstream("gemini", 3, 0, 0)
            )),
            t0,
        );
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b", "peer-c"]), t0);
        let fz = m.filter_provider("zen").expect("zen exists");
        assert_eq!(fz.columns.len(), 1);
        assert_eq!(fz.columns[0].upstream, "zen");
        assert_eq!(fz.hidden_columns, 0);
        assert_eq!(
            fz.rows.iter().map(|r| r.peer.as_str()).collect::<Vec<_>>(),
            ["peer-a", "peer-c"],
            "peer-b is absent for zen"
        );
        assert_eq!(fz.rows[0].cells, vec![Cell::Healthy]);
        assert_eq!(fz.rows[1].cells, vec![Cell::Healthy]);
        assert_eq!(fz.columns[0].peers_with, 2);
        assert_eq!(fz.columns[0].healthy_peers, 2);
    }

    #[test]
    fn filter_falls_back_when_the_provider_vanishes() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(&mut f, "peer-a", &doc(&upstream("zen", 1, 0, 0)), t0);
        let m = Matrix::build(&f, &names(&["peer-a"]), t0);
        assert!(m.filter_provider("gone").is_none());
    }
}

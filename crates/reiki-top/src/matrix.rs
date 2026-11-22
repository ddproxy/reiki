//! Pure model for the matrix view: peers x providers, no drawing. A peer that does not use a
//! provider is `Absent`, never a failure, because peers publish different provider sets.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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

/// Minutes a publisher lists per upstream, the ten ending at its current minute.
pub const MINUTE_SLOTS: usize = 10;

/// The pseudo-peer that aggregates every fresh peer in the chart and the peers list.
pub const FLEET_PEER: &str = "ALL";

/// Points drawn in a usage chart window.
pub const CHART_POINTS: usize = 12;
/// The publisher's one-minute usage window.
pub const MINUTE_INTERVAL: Duration = Duration::from_secs(60);
/// The publisher's five-minute usage window.
pub const BUCKET_INTERVAL: Duration = Duration::from_secs(300);

/// Whether this is the fleet's pseudo-row rather than a configured peer.
pub fn is_fleet_row(row: &Row) -> bool {
    row.peer == FLEET_PEER
}

/// One upstream's attempts and tokens in the last hour, from a publisher that exposes usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pressure {
    pub requests_1h: u64,
    pub limited_1h: u32,
    pub tokens_1h: u64,
    /// Twelve five-minute request counts, oldest first, newest last.
    pub buckets: [u32; 12],
    /// Ten one-minute request counts, oldest first; `None` when usage is not exposed.
    pub minutes: Option<[u32; MINUTE_SLOTS]>,
    /// Twelve five-minute token counts, oldest first; `None` when usage is not exposed.
    pub token_buckets: Option<[u64; 12]>,
    /// Ten one-minute token counts, oldest first; `None` when usage is not exposed.
    pub token_minutes: Option<[u32; MINUTE_SLOTS]>,
}

/// One peer row: a cell per column, in column order.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub peer: String,
    pub cells: Vec<Cell>,
    /// Cooling combinations per column, in column order; 0 where the peer has no such upstream
    /// or its data is stale.
    pub combos: Vec<u32>,
    /// Attempt counts per column, in column order; `None` when usage is not exposed, or for a
    /// stale or absent upstream.
    pub pressure: Vec<Option<Pressure>>,
    pub host: HostSummary,
    /// The peer's published version from its last data, stale or not; `None` for a peer never
    /// seen.
    pub version: Option<String>,
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
        let mut with: HashMap<String, usize> = HashMap::new();
        for name in peers {
            let Some(view) = fleet.peer(name, now) else {
                continue;
            };
            let mut seen = HashSet::new();
            for upstream in display_upstreams(view.health) {
                if seen.insert(upstream.name.clone()) {
                    *with.entry(upstream.name).or_insert(0) += 1;
                }
            }
        }
        let mut columns: Vec<Column> = with
            .into_iter()
            .map(|(upstream, peers_with)| Column {
                upstream,
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
            let upstreams = view
                .as_ref()
                .map(|v| display_upstreams(v.health))
                .unwrap_or_default();
            let upstream = |col: &Column| {
                fresh.and_then(|_| upstreams.iter().find(|u| u.name == col.upstream))
            };
            let has_upstream = |col: &Column| upstreams.iter().any(|u| u.name == col.upstream);
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
                pressure: columns
                    .iter()
                    .map(|col| upstream(col).and_then(pressure_of))
                    .collect(),
                host,
                version: view.as_ref().map(|v| v.health.reiki_version.clone()),
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
            .filter(|r| is_fleet_row(r) || r.cells.get(idx) != Some(&Cell::Absent))
            .map(|r| Row {
                peer: r.peer.clone(),
                cells: vec![r.cells[idx].clone()],
                combos: vec![r.combos.get(idx).copied().unwrap_or(0)],
                pressure: vec![r.pressure.get(idx).copied().flatten()],
                host: r.host.clone(),
                version: r.version.clone(),
                error: r.error.clone(),
                stale: r.stale,
                age: r.age,
            })
            .collect();
        let real: Vec<&Row> = rows.iter().filter(|r| !is_fleet_row(r)).collect();
        let healthy_peers = real
            .iter()
            .filter(|r| r.cells.first() == Some(&Cell::Healthy))
            .count();
        let column = Column {
            upstream: self.columns[idx].upstream.clone(),
            peers_with: real.len(),
            healthy_peers,
        };
        Some(Matrix {
            columns: vec![column],
            rows,
            hidden_columns: 0,
        })
    }

    /// [`Matrix::build`] with the pseudo-peer `ALL` prepended. The fleet row sums the fresh
    /// peers' usage and carries the worst key state of each provider; configured peer rows are
    /// otherwise unchanged. Provider tallies and counts never include the pseudo-peer.
    pub fn build_fleet(fleet: &Fleet, peers: &[String], now: Instant) -> Matrix {
        let base = Self::build(fleet, peers, now);
        let fleet_row = Self::fleet_row(&base);
        let mut rows = Vec::with_capacity(base.rows.len() + 1);
        if !base.rows.iter().any(is_fleet_row) {
            rows.push(fleet_row);
        }
        rows.extend(base.rows);
        Matrix {
            columns: base.columns,
            rows,
            hidden_columns: base.hidden_columns,
        }
    }

    /// The pseudo-row, from real peer rows only. Stale peers are excluded here; the caller counts
    /// them separately in the chart title.
    fn fleet_row(base: &Matrix) -> Row {
        let fresh: Vec<&Row> = base.rows.iter().filter(|r| !r.stale).collect();
        let cells: Vec<Cell> = (0..base.columns.len())
            .map(|c| {
                merge_cells(
                    fresh
                        .iter()
                        .filter_map(|r| r.cells.get(c))
                        .filter(|cell| **cell != Cell::Absent && **cell != Cell::Stale),
                )
            })
            .collect();
        let combos = (0..base.columns.len())
            .map(|c| {
                fresh
                    .iter()
                    .filter_map(|r| r.combos.get(c))
                    .fold(0u32, |sum, &n| sum.saturating_add(n))
            })
            .collect();
        let pressure = (0..base.columns.len())
            .map(|c| {
                let present: Vec<&Pressure> = fresh
                    .iter()
                    .filter_map(|r| r.pressure.get(c).and_then(Option::as_ref))
                    .collect();
                merge_pressures(&present)
            })
            .collect();
        let host = HostSummary {
            load1: fresh.iter().filter_map(|r| r.host.load1).reduce(f64::max),
            disk_percent: fresh
                .iter()
                .filter_map(|r| r.host.disk_percent)
                .reduce(f64::max),
            sessions: fresh
                .iter()
                .filter_map(|r| r.host.sessions)
                .reduce(u64::saturating_add),
        };
        Row {
            peer: FLEET_PEER.to_owned(),
            cells,
            combos,
            pressure,
            host,
            version: None,
            error: None,
            stale: fresh.is_empty(),
            age: fresh.iter().filter_map(|r| r.age).min(),
        }
    }

    /// `(fresh, total)` over configured peers. The `ALL` row is not a peer and never counts.
    pub fn fresh_counts(&self) -> (usize, usize) {
        let real: Vec<&Row> = self.rows.iter().filter(|r| !is_fleet_row(r)).collect();
        (real.iter().filter(|r| !r.stale).count(), real.len())
    }
}

/// Display-only upstream entries for observed passthrough accounts. They carry no key state:
/// one synthetic entry stands for the account, marked through `capacity_group`, so a reader
/// cannot mistake it for a failover-capable key.
fn observed_upstreams(health: &Health) -> Vec<Upstream> {
    let mut by_name: BTreeMap<String, (u8, Upstream)> = BTreeMap::new();
    for entry in &health.observed {
        let rank = observed_rank(&entry.status_class);
        let (healthy, cooling, disabled) = match rank {
            3 => (0, 0, 1),
            2 => (0, 1, 0),
            _ => (1, 0, 0),
        };
        let candidate = Upstream {
            name: entry.upstream.clone(),
            healthy_keys: healthy,
            cooling_keys: cooling,
            disabled_keys: disabled,
            cooling_combinations: 0,
            soonest_cooldown_seconds: if cooling > 0 {
                entry.retry_after_seconds
            } else {
                None
            },
            last_status_class: Some(entry.status_class.clone()),
            capacity_group: Some(format!("observed/{}", entry.account)),
            keys: None,
            requests_1h: None,
            limited_1h: None,
            request_buckets: None,
            tokens_1h: None,
            token_buckets: None,
            request_minutes: None,
            token_minutes: None,
        };
        match by_name.get_mut(&entry.upstream) {
            Some((current, _)) if *current >= rank => {}
            Some((current, existing)) => {
                *current = rank;
                *existing = candidate;
            }
            None => {
                by_name.insert(entry.upstream.clone(), (rank, candidate));
            }
        }
    }
    by_name
        .into_values()
        .map(|(_, upstream)| upstream)
        .collect()
}

fn observed_rank(class: &str) -> u8 {
    match class {
        "credential_rejected" | "access_disabled" => 3,
        "rate_limited" | "quota_exhausted" => 2,
        _ => 1,
    }
}

fn display_upstreams(health: &Health) -> Vec<Upstream> {
    let mut out = health.upstreams.clone();
    let named: HashSet<String> = out.iter().map(|u| u.name.clone()).collect();
    out.extend(
        observed_upstreams(health)
            .into_iter()
            .filter(|u| !named.contains(&u.name)),
    );
    out
}

/// Cell severity, for the fleet row's "worst state" rule.
fn cell_rank(cell: &Cell) -> u8 {
    match cell {
        Cell::Absent => 0,
        Cell::Healthy => 1,
        Cell::Partial { .. } => 2,
        Cell::Cooling { .. } => 3,
        Cell::Disabled => 4,
        Cell::Stale => 5,
    }
}

/// Keeps the worst state; equal states keep the soonest cooldown and summed key counts.
fn merge_cells<'a>(cells: impl Iterator<Item = &'a Cell>) -> Cell {
    let mut out = Cell::Absent;
    for cell in cells {
        match (&mut out, cell) {
            (Cell::Absent, _) => out = cell.clone(),
            (cur, next) if cell_rank(next) > cell_rank(cur) => out = next.clone(),
            (
                Cell::Partial {
                    cooling,
                    total,
                    soonest,
                },
                Cell::Partial {
                    cooling: n,
                    total: t,
                    soonest: s,
                },
            ) => {
                *cooling = cooling.saturating_add(*n);
                *total = total.saturating_add(*t);
                *soonest = sooner(*soonest, *s);
            }
            (
                Cell::Cooling { soonest, class },
                Cell::Cooling {
                    soonest: s,
                    class: c,
                },
            ) => {
                *soonest = sooner(*soonest, *s);
                if class.is_none() {
                    *class = c.clone();
                }
            }
            _ => {}
        }
    }
    out
}

fn sooner(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Adds fresh peers' usage buckets. A missing minute series becomes zeros, never a dropped
/// interval, so the newest points still line up at the right edge.
fn merge_pressures(pressures: &[&Pressure]) -> Option<Pressure> {
    if pressures.is_empty() {
        return None;
    }
    let mut out = Pressure {
        requests_1h: 0,
        limited_1h: 0,
        tokens_1h: 0,
        buckets: [0; 12],
        minutes: None,
        token_buckets: None,
        token_minutes: None,
    };
    let mut minutes = [0u32; MINUTE_SLOTS];
    let mut token_buckets = [0u64; 12];
    let mut token_minutes = [0u32; MINUTE_SLOTS];
    let mut has_minutes = false;
    let mut has_token_buckets = false;
    let mut has_token_minutes = false;
    for p in pressures {
        out.requests_1h = out.requests_1h.saturating_add(p.requests_1h);
        out.limited_1h = out.limited_1h.saturating_add(p.limited_1h);
        out.tokens_1h = out.tokens_1h.saturating_add(p.tokens_1h);
        for (sum, bucket) in out.buckets.iter_mut().zip(p.buckets) {
            *sum = sum.saturating_add(bucket);
        }
        if let Some(values) = p.minutes {
            has_minutes = true;
            for (sum, value) in minutes.iter_mut().zip(values) {
                *sum = sum.saturating_add(value);
            }
        }
        if let Some(values) = p.token_buckets {
            has_token_buckets = true;
            for (sum, value) in token_buckets.iter_mut().zip(values) {
                *sum = sum.saturating_add(value);
            }
        }
        if let Some(values) = p.token_minutes {
            has_token_minutes = true;
            for (sum, value) in token_minutes.iter_mut().zip(values) {
                *sum = sum.saturating_add(value);
            }
        }
    }
    out.minutes = has_minutes.then_some(minutes);
    out.token_buckets = has_token_buckets.then_some(token_buckets);
    out.token_minutes = has_token_minutes.then_some(token_minutes);
    Some(out)
}

/// Display order: `ALL` first, then problem peers (stale, or any `Partial`/`Cooling`/`Disabled`
/// cell), then by name. A peer with only `Absent` and `Healthy` cells is not a problem.
pub fn order_peers(matrix: &Matrix) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..matrix.rows.len()).collect();
    idx.sort_by(|&a, &b| {
        let (ra, rb) = (&matrix.rows[a], &matrix.rows[b]);
        is_fleet_row(rb)
            .cmp(&is_fleet_row(ra))
            .then_with(|| is_problem(rb).cmp(&is_problem(ra)))
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
    if is_fleet_row(row) {
        return false;
    }
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

    /// The raw values of one metric over the retained window, oldest first. Samples missing the
    /// metric are skipped.
    pub fn values(&self, peer: &str, metric: Metric) -> Vec<f64> {
        self.peers.get(peer).map_or_else(Vec::new, |ring| {
            ring.samples
                .iter()
                .filter_map(|s| value(s, metric))
                .collect()
        })
    }

    /// Levels 0..=8 over the retained window. Samples missing the metric are skipped, so the
    /// series can be shorter than the window. Scaling is stated on [`Metric`].
    pub fn series(&self, peer: &str, metric: Metric) -> Vec<u64> {
        let values = self.values(peer, metric);
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

/// All the core figures arrive together when a publisher exposes usage; absence means there is
/// nothing to chart, not a zero to invent.
fn pressure_of(upstream: &Upstream) -> Option<Pressure> {
    Some(Pressure {
        requests_1h: upstream.requests_1h?,
        limited_1h: upstream.limited_1h?,
        tokens_1h: upstream.tokens_1h?,
        buckets: upstream.request_buckets?,
        minutes: upstream.request_minutes,
        token_buckets: upstream.token_buckets,
        token_minutes: upstream.token_minutes,
    })
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

/// The peers' values of one metric summed by sample age, so the newest samples line up at the
/// right edge. Disk takes the fullest peer instead of a sum.
pub fn fleet_series(history: &History, peers: &[String], metric: Metric) -> Vec<Option<f64>> {
    let series: Vec<Vec<f64>> = peers
        .iter()
        .map(|peer| history.values(peer, metric))
        .collect();
    let len = series.iter().map(Vec::len).max().unwrap_or(0);
    let mut out: Vec<Option<f64>> = vec![None; len];
    for values in &series {
        for (age, v) in values.iter().rev().enumerate() {
            let slot = &mut out[len - 1 - age];
            *slot = Some(match (*slot, metric) {
                (None, _) => *v,
                (Some(acc), Metric::DiskPercent) => acc.max(*v),
                (Some(acc), _) => acc + v,
            });
        }
    }
    out
}

/// Requests per five-minute bucket, summed over the rows' providers. `None` for every bucket
/// when no row publishes request figures.
pub fn request_series(rows: &[Row]) -> Vec<Option<f64>> {
    let mut sums = [0u64; 12];
    let mut published = false;
    for pressure in rows.iter().flat_map(|row| row.pressure.iter().flatten()) {
        published = true;
        for (sum, bucket) in sums.iter_mut().zip(pressure.buckets) {
            *sum += u64::from(bucket);
        }
    }
    if !published {
        return vec![None; sums.len()];
    }
    sums.iter().map(|&n| Some(n as f64)).collect()
}

/// The figure a provider-coloured usage chart plots. `u` cycles this alongside the gauge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UsageMetric {
    #[default]
    Requests,
    Limited,
    Tokens,
}

impl UsageMetric {
    /// The next `u` position.
    pub fn next(self) -> Self {
        match self {
            Self::Requests => Self::Limited,
            Self::Limited => Self::Tokens,
            Self::Tokens => Self::Requests,
        }
    }

    /// The legend/title name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Requests => "requests",
            Self::Limited => "limited",
            Self::Tokens => "tokens",
        }
    }

    /// The chart title's name.
    pub fn title(self) -> &'static str {
        match self {
            Self::Requests => "Requests",
            Self::Limited => "Limited",
            Self::Tokens => "Tokens",
        }
    }

    /// The unit shown in the chart title.
    pub fn unit(self) -> &'static str {
        match self {
            Self::Requests => "req/interval",
            Self::Limited => "limited/interval",
            Self::Tokens => "tok/interval",
        }
    }
}

/// Usage points with the time each point covers. The chart's x axis is derived from these,
/// never from the point indices.
#[derive(Debug, Clone, PartialEq)]
pub struct TimedSeries {
    pub interval: Duration,
    pub points: Vec<f64>,
}

/// One provider's series, in provider-column order.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSeries {
    pub upstream: String,
    pub series: TimedSeries,
}

/// Aligns series on their newest point and sums them. Different intervals are resolved by
/// splitting a coarse point evenly across the fine points it covers; missing points are zero.
pub fn align_and_sum(series: &[TimedSeries]) -> TimedSeries {
    let Some(interval) = series.iter().map(|s| s.interval).min() else {
        return TimedSeries {
            interval: MINUTE_INTERVAL,
            points: Vec::new(),
        };
    };
    let fine = interval.as_secs().max(1);
    let ratio_of = |s: &TimedSeries| {
        usize::try_from((s.interval.as_secs().max(fine) / fine).max(1)).unwrap_or(usize::MAX)
    };
    let len = series
        .iter()
        .map(|s| s.points.len().saturating_mul(ratio_of(s)))
        .max()
        .unwrap_or(0);
    let mut out = vec![0.0; len];
    for s in series {
        let ratio = ratio_of(s);
        for (age, value) in s.points.iter().rev().enumerate() {
            let right = len.saturating_sub(age.saturating_mul(ratio));
            let left = right.saturating_sub(ratio);
            for slot in out.iter_mut().take(right).skip(left) {
                *slot += *value / ratio as f64;
            }
        }
    }
    TimedSeries {
        interval,
        points: out,
    }
}

fn minute_points(values: Option<[u32; MINUTE_SLOTS]>) -> Option<Vec<f64>> {
    values.map(|minutes| {
        let mut points = vec![0.0; CHART_POINTS - MINUTE_SLOTS];
        points.extend(minutes.iter().map(|&n| f64::from(n)));
        points
    })
}

fn token_minute_points(values: Option<[u32; MINUTE_SLOTS]>) -> Option<Vec<f64>> {
    values.map(|minutes| {
        let mut points = vec![0.0; CHART_POINTS - MINUTE_SLOTS];
        points.extend(minutes.iter().map(|&n| f64::from(n)));
        points
    })
}

fn bucket_points(values: [u32; 12]) -> Vec<f64> {
    values.iter().map(|&n| f64::from(n)).collect()
}

fn token_points(pressure: &Pressure, interval: Duration) -> Vec<f64> {
    if interval == MINUTE_INTERVAL {
        token_minute_points(pressure.token_minutes).unwrap_or_else(|| vec![0.0; CHART_POINTS])
    } else {
        pressure
            .token_buckets
            .as_ref()
            .map(|values| values.iter().map(|&n| n as f64).collect())
            .unwrap_or_else(|| vec![0.0; CHART_POINTS])
    }
}

/// One pressure's points for a metric. Limited has no bucket series in health: it is derived
/// from the hour's limited/request ratio, spread evenly when no requests were reported.
fn usage_points(pressure: &Pressure, metric: UsageMetric, interval: Duration) -> Vec<f64> {
    match metric {
        UsageMetric::Requests => {
            if interval == MINUTE_INTERVAL {
                minute_points(pressure.minutes).unwrap_or_else(|| bucket_points(pressure.buckets))
            } else {
                bucket_points(pressure.buckets)
            }
        }
        UsageMetric::Limited => {
            let ratio = if pressure.requests_1h > 0 {
                f64::from(pressure.limited_1h) / pressure.requests_1h as f64
            } else if pressure.limited_1h > 0 {
                f64::from(pressure.limited_1h) / CHART_POINTS as f64
            } else {
                0.0
            };
            let base = if interval == MINUTE_INTERVAL {
                minute_points(pressure.minutes).unwrap_or_else(|| bucket_points(pressure.buckets))
            } else {
                bucket_points(pressure.buckets)
            };
            base.iter().map(|v| v * ratio).collect()
        }
        UsageMetric::Tokens => token_points(pressure, interval),
    }
}

/// Provider-coloured chart series for the fresh rows of `matrix`, in column order. Minute data
/// wins when any fresh peer publishes it; otherwise the hour's five-minute buckets are used.
/// A provider with no pressure at all still gets a zero series so its name and colour appear.
pub fn usage_by_provider(matrix: &Matrix, metric: UsageMetric) -> Vec<ProviderSeries> {
    (0..matrix.columns.len())
        .filter_map(|c| {
            let has_upstream = matrix.rows.iter().any(|row| {
                !is_fleet_row(row) && !row.stale && row.cells.get(c) != Some(&Cell::Absent)
            });
            if !has_upstream {
                return None;
            }
            let pressures: Vec<&Pressure> = matrix
                .rows
                .iter()
                .filter(|row| {
                    !is_fleet_row(row) && !row.stale && row.cells.get(c) != Some(&Cell::Absent)
                })
                .filter_map(|row| row.pressure.get(c).and_then(Option::as_ref))
                .collect();
            let use_minutes = match metric {
                UsageMetric::Requests => pressures.iter().any(|p| p.minutes.is_some()),
                UsageMetric::Tokens => pressures.iter().any(|p| p.token_minutes.is_some()),
                UsageMetric::Limited => pressures.iter().any(|p| p.minutes.is_some()),
            };
            let interval = if use_minutes {
                MINUTE_INTERVAL
            } else {
                BUCKET_INTERVAL
            };
            let series: Vec<TimedSeries> = if pressures.is_empty() {
                vec![TimedSeries {
                    interval,
                    points: vec![0.0; CHART_POINTS],
                }]
            } else {
                pressures
                    .iter()
                    .filter(|p| match metric {
                        UsageMetric::Tokens => {
                            use_minutes == p.token_minutes.is_some()
                                || (!use_minutes && p.token_buckets.is_some())
                        }
                        _ => use_minutes == p.minutes.is_some(),
                    })
                    .map(|p| TimedSeries {
                        interval,
                        points: usage_points(p, metric, interval),
                    })
                    .collect()
            };
            let series = align_and_sum(&series);
            Some(ProviderSeries {
                upstream: matrix.columns[c].upstream.clone(),
                series,
            })
        })
        .collect()
}

/// Keys by state, summed over the fresh peers and every provider: healthy, cooling, disabled.
pub fn key_counts(fleet: &Fleet, peers: &[String], now: Instant) -> [u64; 3] {
    let mut counts = [0u64; 3];
    for name in peers {
        let Some(peer) = fleet.peer(name, now).filter(|p| !p.stale) else {
            continue;
        };
        for upstream in &peer.health.upstreams {
            counts[0] += u64::from(upstream.healthy_keys);
            counts[1] += u64::from(upstream.cooling_keys);
            counts[2] += u64::from(upstream.disabled_keys);
        }
    }
    counts
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
            r#"{{"name":"{name}","healthy_keys":{healthy},"cooling_keys":{cooling},"disabled_keys":{disabled},
                "cooling_combinations":0,"soonest_cooldown_seconds":null,"last_status_class":null}}"#
        )
    }

    fn usage_upstream(name: &str, requests: u64, limited: u32, buckets: &str) -> String {
        format!(
            r#"{{"name":"{name}","healthy_keys":1,"cooling_keys":0,"disabled_keys":0,
                "cooling_combinations":0,"soonest_cooldown_seconds":null,"last_status_class":"2xx",
                "requests_1h":{requests},"limited_1h":{limited},
                "request_buckets":{buckets},"tokens_1h":0,
                "token_buckets":[0,0,0,0,0,0,0,0,0,0,0,0],
                "request_minutes":[0,0,0,0,0,0,0,0,0,0],
                "token_minutes":[0,0,0,0,0,0,0,0,0,0]}}"#
        )
    }

    fn doc(upstreams: &str) -> String {
        format!(
            r#"{{"instance":"x","reiki_version":"0.10.3","observed_at":"t","age_seconds":0,
                "observed" : [],
                "failures":{{}},"upstreams":[{upstreams}]}}"#
        )
    }

    fn names(peers: &[&str]) -> Vec<String> {
        peers.iter().map(|p| p.to_string()).collect()
    }

    macro_rules! doc {
        ($name:literal) => {
            doc(&upstream($name, 0, 0, 0))
        };
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
            &doc(r#"{
   "cooling_combinations" : 0,
   "cooling_keys" : 1,
   "disabled_keys" : 0,
   "healthy_keys" : 1,
   "keys" : [
      {
         "cooldown_class" : "quota_exhausted",
         "cooldown_seconds" : 300,
         "name" : "b",
         "state" : "cooling"
      }
   ],
   "last_status_class" : "4xx",
   "name" : "zen",
   "soonest_cooldown_seconds" : 300
}
"#),
            t0,
        );
        add(
            &mut f,
            "peer-b",
            &doc(r#"{
   "cooling_combinations" : 0,
   "cooling_keys" : 2,
   "disabled_keys" : 0,
   "healthy_keys" : 0,
   "keys" : [
      {
         "cooldown_class" : "rate_limited",
         "cooldown_seconds" : 300,
         "name" : "a",
         "state" : "cooling"
      },
      {
         "cooldown_class" : "quota_exhausted",
         "cooldown_seconds" : 42,
         "name" : "b",
         "state" : "cooling"
      }
   ],
   "last_status_class" : "4xx",
   "name" : "zen",
   "soonest_cooldown_seconds" : 42
}
"#),
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
    fn observed_accounts_get_a_display_only_column() {
        let t0 = Instant::now();
        let mut f = fleet();
        let doc = r#"{"instance":"x","reiki_version":"0.10.3","observed_at":"t","age_seconds":0,
            "observed":[{"upstream":"oauth","account":"alice","mode":"observed",
            "status_class":"rate_limited","observed_at":"t","retry_after_seconds":20,
            "cooldown_class":"rate_limited","quota":{"requests_limit":100,"requests_remaining":0,
            "requests_reset_seconds":30,"tokens_limit":null,"tokens_remaining":null,
            "tokens_reset_seconds":null}}],"failures":{},"upstreams":[]}"#;
        add(&mut f, "peer-a", doc, t0);
        let m = Matrix::build(&f, &names(&["peer-a"]), t0);
        assert_eq!(m.columns.len(), 1);
        assert_eq!(m.columns[0].upstream, "oauth");
        assert_eq!(
            m.rows[0].cells,
            vec![Cell::Cooling {
                soonest: Some(Duration::from_secs(20)),
                class: None,
            }],
            "an observed cooldown is visible but is not a key state"
        );
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
    fn pressure_is_carried_per_column_and_none_when_absent_or_stale() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &doc(&usage_upstream(
                "zen",
                120,
                3,
                "[1,2,3,4,5,6,7,8,9,10,11,12]",
            )),
            t0,
        );
        add(&mut f, "peer-b", &doc(&upstream("zen", 1, 0, 0)), t0);
        add(
            &mut f,
            "peer-c",
            &doc(&usage_upstream("zen", 9, 0, "[1,2,3,4,5,6,7,8,9,10,11,9]")),
            t0 - Duration::from_secs(91),
        );
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b", "peer-c"]), t0);
        assert_eq!(
            m.rows[0].pressure,
            vec![Some(Pressure {
                requests_1h: 120,
                limited_1h: 3,
                tokens_1h: 0,
                buckets: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                minutes: Some([0; 10]),
                token_buckets: Some([0; 12]),
                token_minutes: Some([0; 10]),
            })],
            "exposed figures reach the row"
        );
        assert_eq!(
            m.rows[1].pressure,
            vec![None],
            "a publisher without exposure has none"
        );
        assert_eq!(m.rows[2].pressure, vec![None], "stale data counts nothing");

        let filtered = m.filter_provider("zen").expect("zen exists");
        assert_eq!(
            filtered.rows[0].pressure, m.rows[0].pressure,
            "filtering keeps the column's pressure"
        );
        assert_eq!(filtered.rows[1].pressure, vec![None]);
    }

    #[test]
    fn host_summary_takes_worst_disk_and_survives_hostile_json() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "host" : {
      "cores" : 8,
      "disks" : [
         {
            "used_percent" : 40
         },
         {
            "used_percent" : 90.5
         },
         {
            "used_percent" : 10
         }
      ],
      "load_average" : [
         0.5,
         1.5
      ]
   },
   "instance" : "x",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "sessions" : {
      "active_sessions" : 3
   },
   "upstreams" : []
}
"#,
            t0,
        );
        add(
            &mut f,
            "peer-b",
            r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "host" : "not an object",
   "instance" : "x",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "sessions" : {
      "active_sessions" : "three"
   },
   "upstreams" : []
}
"#,
            t0,
        );
        add(
            &mut f,
            "peer-c",
            r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "host" : {
      "disks" : [
         {
            "used_percent" : "high"
         },
         {
            "used_percent" : 150
         },
         {
            "used_percent" : -5
         }
      ],
      "load_average" : [
         null,
         -1,
         1e+308
      ]
   },
   "instance" : "x",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "sessions" : {
      "active_sessions" : 1e+30
   },
   "upstreams" : []
}
"#,
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
            r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "x",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 0,
         "disabled_keys" : 0,
         "healthy_keys" : 0,
         "keys" : [
            {
               "cooldown_class" : null,
               "cooldown_seconds" : null,
               "name" : "a",
               "state" : "cooling"
            },
            {
               "cooldown_class" : null,
               "cooldown_seconds" : 5,
               "name" : "b",
               "state" : "weird"
            }
         ],
         "last_status_class" : null,
         "name" : "zen",
         "soonest_cooldown_seconds" : 18446744073709551615
      }
   ]
}
"#,
            t0,
        );
        add(&mut f, "peer-a", &doc(&upstream("zen", 1, 1, 1)), t0);
        add(
            &mut f,
            "peer-a",
            r#"{
   "age_seconds" : 0,
   "observed" : [],
   "failures" : {},
   "instance" : "x",
   "observed_at" : "t",
   "reiki_version" : "0.10.3",
   "upstreams" : [
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 1,
         "disabled_keys" : 0,
         "healthy_keys" : 1,
         "last_status_class" : null,
         "name" : "zen",
         "soonest_cooldown_seconds" : null
      },
      {
         "cooling_combinations" : 0,
         "cooling_keys" : 5,
         "disabled_keys" : 0,
         "healthy_keys" : 0,
         "last_status_class" : null,
         "name" : "zen",
         "soonest_cooldown_seconds" : 7
      }
   ]
}
"#,
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

    fn sample(load: f64, disk: f64, sessions: u64) -> Sample {
        Sample {
            load1: Some(load),
            disk_percent: Some(disk),
            sessions: Some(sessions),
        }
    }

    #[test]
    fn raw_values_are_the_unscaled_samples_oldest_first() {
        let mut h = History::new(60);
        h.push("peer-a", sample(0.5, 40.0, 3));
        h.push("peer-a", Sample::default());
        h.push("peer-a", sample(2.5, 60.0, 7));
        assert_eq!(h.values("peer-a", Metric::Load1), vec![0.5, 2.5]);
        assert_eq!(h.values("peer-a", Metric::Sessions), vec![3.0, 7.0]);
        assert!(h.values("nobody", Metric::Load1).is_empty());
    }

    #[test]
    fn fleet_load_sums_by_age_and_disk_takes_the_fullest_peer() {
        let mut h = History::new(60);
        h.push("peer-a", sample(1.0, 30.0, 1));
        h.push("peer-a", sample(2.0, 40.0, 1));
        h.push("peer-b", sample(4.0, 90.0, 1));
        let peers = names(&["peer-a", "peer-b"]);
        assert_eq!(
            fleet_series(&h, &peers, Metric::Load1),
            vec![Some(1.0), Some(2.0 + 4.0)],
            "the newest samples line up at the right edge"
        );
        assert_eq!(
            fleet_series(&h, &peers, Metric::DiskPercent),
            vec![Some(30.0), Some(90.0)],
            "the fullest peer at each age"
        );
        assert!(fleet_series(&h, &names(&["nobody"]), Metric::Load1).is_empty());
    }

    #[test]
    fn request_buckets_sum_over_providers_and_stay_gaps_without_figures() {
        let doc = doc(&format!(
            "{},{}",
            usage_upstream("zen", 3, 0, "[1,0,0,0,0,0,0,0,0,0,0,2]"),
            usage_upstream("gemini", 1, 0, "[0,0,0,0,0,0,0,0,0,0,0,5]")
        ));
        let mut f = fleet();
        let t0 = Instant::now();
        add(&mut f, "peer-a", &doc, t0);
        add(&mut f, "peer-b", &doc!("zen"), t0);
        let m = Matrix::build(&f, &names(&["peer-a", "peer-b"]), t0);
        let a = m.rows.iter().find(|r| r.peer == "peer-a").unwrap();
        let series = request_series(std::slice::from_ref(a));
        assert_eq!(series.len(), 12);
        assert_eq!(series[0], Some(1.0));
        assert_eq!(series[11], Some(7.0));
        assert_eq!(series[5], Some(0.0));
        let fleet_total = request_series(&m.rows);
        assert_eq!(fleet_total[11], Some(7.0));
        let none = request_series(&[]);
        assert_eq!(none, vec![None; 12]);
    }

    #[test]
    fn key_counts_cover_fresh_peers_and_every_provider() {
        let mut f = fleet();
        let t0 = Instant::now();
        add(
            &mut f,
            "peer-a",
            &doc(&format!(
                "{},{}",
                upstream("zen", 2, 1, 0),
                upstream("gemini", 1, 0, 1)
            )),
            t0,
        );
        add(&mut f, "peer-b", &doc(&upstream("zen", 0, 3, 0)), t0);
        assert_eq!(key_counts(&f, &names(&["peer-a", "peer-b"]), t0), [3, 4, 1]);
        let later = t0 + Duration::from_secs(600);
        assert_eq!(
            key_counts(&f, &names(&["peer-a", "peer-b"]), later),
            [0, 0, 0],
            "stale peers count for nothing"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn rich_upstream(
        name: &str,
        requests: u64,
        limited: u32,
        tokens: u64,
        healthy: u32,
        cooling: u32,
        minutes: &str,
        token_minutes: &str,
    ) -> String {
        format!(
            r#"{{"name":"{name}","healthy_keys":{healthy},"cooling_keys":{cooling},"disabled_keys":0,
                "cooling_combinations":0,"soonest_cooldown_seconds":null,"last_status_class":null,
                "requests_1h":{requests},"limited_1h":{limited},"tokens_1h":{tokens},
                "request_buckets":[1,1,1,1,1,1,1,1,1,1,1,1],
                "token_buckets":[1,1,1,1,1,1,1,1,1,1,1,1],
                "request_minutes":{minutes},"token_minutes":{token_minutes}}}"#
        )
    }

    #[test]
    fn align_and_sum_aligns_newest_points_and_splits_a_coarse_interval() {
        let fine = TimedSeries {
            interval: MINUTE_INTERVAL,
            points: vec![1.0, 2.0, 3.0],
        };
        let coarse = TimedSeries {
            interval: Duration::from_secs(180),
            points: vec![30.0],
        };
        let summed = align_and_sum(&[fine, coarse]);
        assert_eq!(summed.interval, MINUTE_INTERVAL);
        assert_eq!(
            summed.points,
            vec![11.0, 12.0, 13.0],
            "the newest points align"
        );
    }

    #[test]
    fn align_and_sum_treats_missing_points_as_zero_and_aligns_the_newest() {
        let short = TimedSeries {
            interval: MINUTE_INTERVAL,
            points: vec![1.0, 2.0],
        };
        let long = TimedSeries {
            interval: MINUTE_INTERVAL,
            points: vec![10.0, 20.0, 30.0],
        };
        assert_eq!(align_and_sum(&[short, long]).points, vec![10.0, 21.0, 32.0]);
        assert!(align_and_sum(&[]).points.is_empty());
        assert_eq!(
            align_and_sum(&[TimedSeries {
                interval: MINUTE_INTERVAL,
                points: vec![0.0; CHART_POINTS],
            }])
            .points
            .len(),
            CHART_POINTS
        );
    }

    #[test]
    fn the_fleet_row_sums_fresh_usage_and_carries_the_worst_state() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &doc(&rich_upstream(
                "zen",
                10,
                2,
                100,
                1,
                0,
                "[1,1,1,1,1,1,1,1,1,1]",
                "[1,1,1,1,1,1,1,1,1,1]",
            )),
            t0,
        );
        add(
            &mut f,
            "peer-b",
            &doc(&rich_upstream(
                "zen",
                5,
                1,
                50,
                0,
                1,
                "[1,1,1,1,1,1,1,1,1,1]",
                "[1,1,1,1,1,1,1,1,1,1]",
            )),
            t0,
        );
        add(
            &mut f,
            "peer-c",
            &doc(&rich_upstream(
                "zen",
                100,
                9,
                900,
                1,
                0,
                "[9,9,9,9,9,9,9,9,9,9]",
                "[9,9,9,9,9,9,9,9,9,9]",
            )),
            t0 - Duration::from_secs(91),
        );
        let peers = names(&["peer-a", "peer-b", "peer-c"]);
        let m = Matrix::build_fleet(&f, &peers, t0);
        assert_eq!(m.rows.len(), 4);
        assert_eq!(m.rows[0].peer, FLEET_PEER);
        assert_eq!(order_peers(&m)[0], 0, "ALL is the first display row");
        assert_eq!(m.fresh_counts(), (2, 3));
        assert!(!is_problem(&m.rows[0]));
        assert_eq!(m.columns[0].peers_with, 3, "tallies ignore ALL");
        let pressure = m.rows[0].pressure[0].as_ref().expect("fleet pressure");
        assert_eq!(pressure.requests_1h, 15, "the stale peer is excluded");
        assert_eq!(pressure.limited_1h, 3);
        assert_eq!(pressure.tokens_1h, 150);
        assert_eq!(pressure.buckets, [2; 12]);
        assert!(matches!(m.rows[0].cells[0], Cell::Cooling { .. }));
        let filtered = m.filter_provider("zen").expect("zen");
        assert_eq!(filtered.rows[0].peer, FLEET_PEER);
        assert_eq!(filtered.columns[0].peers_with, 3);
    }

    #[test]
    fn usage_series_cover_every_provider_even_with_different_peer_sets() {
        let t0 = Instant::now();
        let mut f = fleet();
        add(
            &mut f,
            "peer-a",
            &doc(&format!(
                "{},{}",
                rich_upstream(
                    "zen",
                    10,
                    2,
                    20,
                    1,
                    0,
                    "[1,1,1,1,1,1,1,1,1,1]",
                    "[2,2,2,2,2,2,2,2,2,2]",
                ),
                rich_upstream(
                    "worklog",
                    3,
                    1,
                    6,
                    1,
                    0,
                    "[0,0,0,0,0,0,0,0,0,1]",
                    "[0,0,0,0,0,0,0,0,0,1]",
                )
            )),
            t0,
        );
        add(
            &mut f,
            "peer-b",
            &doc(&rich_upstream(
                "zen",
                5,
                1,
                10,
                1,
                0,
                "[0,0,0,0,0,0,0,0,0,2]",
                "[0,0,0,0,0,0,0,0,0,2]",
            )),
            t0,
        );
        add(
            &mut f,
            "peer-c",
            &doc(&rich_upstream(
                "forgejo",
                90,
                0,
                900,
                1,
                0,
                "[9,9,9,9,9,9,9,9,9,9]",
                "[9,9,9,9,9,9,9,9,9,9]",
            )),
            t0 - Duration::from_secs(91),
        );
        let peers = names(&["peer-a", "peer-b", "peer-c"]);
        let m = Matrix::build_fleet(&f, &peers, t0);
        let requests = usage_by_provider(&m, UsageMetric::Requests);
        assert_eq!(
            requests
                .iter()
                .map(|s| s.upstream.as_str())
                .collect::<Vec<_>>(),
            ["zen", "worklog"],
            "the stale-only forgejo is excluded; credential upstreams are kept"
        );
        assert_eq!(requests[0].series.interval, MINUTE_INTERVAL);
        assert_eq!(requests[0].series.points.len(), CHART_POINTS);
        assert_eq!(requests[0].series.points[11], 3.0, "1 + 2 newest minutes");
        assert_eq!(
            requests[0].series.points[0], 0.0,
            "unreported minutes are zero"
        );
        assert_eq!(requests[1].series.points[11], 1.0);
        let limited = usage_by_provider(&m, UsageMetric::Limited);
        assert!(
            (limited[0].series.points[11] - 0.6).abs() < 1e-9,
            "2/10 of 3"
        );
        let tokens = usage_by_provider(&m, UsageMetric::Tokens);
        assert_eq!(tokens[0].series.points[11], 4.0, "2 + 2 newest minutes");

        let empty = Matrix::build_fleet(&f, &[], t0);
        assert!(usage_by_provider(&empty, UsageMetric::Requests).is_empty());
    }
}

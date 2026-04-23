use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const CACHE_MAGIC: &[u8; 8] = b"ETOPCACH";
const CACHE_VERSION_V2: u16 = 2;
const CACHE_VERSION: u16 = 3;
const CACHE_FILE_NAME: &str = "session-cache.v1.bin";
const QUICK_HYDRATE_GAP_MULTIPLIER: u64 = 3;

const MAX_LIVE_POWER_POINTS: usize = 16_384;
const MAX_LIVE_SNAPSHOTS: usize = 16_384;
const MAX_SNAPSHOT_ROWS: usize = 65_536;
const MAX_TIER_SAMPLES: usize = 16_384;
const MAX_GROUPS_PER_SAMPLE: usize = 65_536;
const MAX_STRING_BYTES: usize = 4_096;
const MAX_REASONABLE_PERSISTED_POWER: f64 = 10_000.0;

pub const RAW_2S_BUCKET_SECS: u64 = 2;
pub const RAW_2S_CAPACITY: usize = 900; // 30m @ 2s
pub const AGG_10S_BUCKET_SECS: u64 = 10;
pub const AGG_10S_CAPACITY: usize = 1_080; // 3h @ 10s
pub const AGG_60S_BUCKET_SECS: u64 = 60;
pub const AGG_60S_CAPACITY: usize = 720; // 12h @ 60s

#[derive(Debug, Clone, PartialEq)]
pub struct LiveProcessSample {
    pub pid: i32,
    pub process: String,
    pub power: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveSnapshot {
    pub tick: u64,
    pub samples: Vec<LiveProcessSample>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupPowerSum {
    pub name: String,
    pub power_sum: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TierSample {
    pub bucket_start_secs: u64,
    pub sample_count: u32,
    pub total_power_sum: f64,
    pub groups: Vec<GroupPowerSum>,
    pub gap_before: bool,
}

impl TierSample {
    fn from_single_sample(
        bucket_start_secs: u64,
        total_power: f64,
        groups: &[GroupPowerSum],
        gap_before: bool,
    ) -> Self {
        Self {
            bucket_start_secs,
            sample_count: 1,
            total_power_sum: total_power,
            groups: groups.to_vec(),
            gap_before,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ArchiveState {
    pub raw_2s: VecDeque<TierSample>,
    pub agg_10s: VecDeque<TierSample>,
    pub agg_60s: VecDeque<TierSample>,
    pub last_sample_unix_secs: Option<u64>,
}

impl ArchiveState {
    pub fn record_sample<I>(
        &mut self,
        timestamp_secs: u64,
        max_contiguous_gap_secs: u64,
        total_power: f64,
        grouped: I,
    ) where
        I: IntoIterator<Item = (String, f64)>,
    {
        let groups = normalize_group_sums(grouped);
        let has_gap_from_previous = self
            .last_sample_unix_secs
            .map(|previous| timestamp_secs.saturating_sub(previous) > max_contiguous_gap_secs)
            .unwrap_or(false);

        upsert_aggregate_bucket(
            &mut self.raw_2s,
            RAW_2S_BUCKET_SECS,
            RAW_2S_CAPACITY,
            timestamp_secs,
            total_power,
            &groups,
            has_gap_from_previous,
        );

        upsert_aggregate_bucket(
            &mut self.agg_10s,
            AGG_10S_BUCKET_SECS,
            AGG_10S_CAPACITY,
            timestamp_secs,
            total_power,
            &groups,
            has_gap_from_previous,
        );

        upsert_aggregate_bucket(
            &mut self.agg_60s,
            AGG_60S_BUCKET_SECS,
            AGG_60S_CAPACITY,
            timestamp_secs,
            total_power,
            &groups,
            has_gap_from_previous,
        );

        self.last_sample_unix_secs = Some(timestamp_secs);
    }

    pub fn enforce_bounds(&mut self) {
        trim_to_capacity(&mut self.raw_2s, RAW_2S_CAPACITY);
        trim_to_capacity(&mut self.agg_10s, AGG_10S_CAPACITY);
        trim_to_capacity(&mut self.agg_60s, AGG_60S_CAPACITY);
    }

    pub fn sanitize_power_data(&mut self, max_reasonable_power: f64) {
        sanitize_tier_power(&mut self.raw_2s, max_reasonable_power);
        sanitize_tier_power(&mut self.agg_10s, max_reasonable_power);
        sanitize_tier_power(&mut self.agg_60s, max_reasonable_power);
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.raw_2s.is_empty() && self.agg_10s.is_empty() && self.agg_60s.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionCache {
    pub saved_at_unix_millis: u64,
    pub last_tick: u64,
    pub live_power_history: Vec<f64>,
    pub live_snapshots: Vec<LiveSnapshot>,
    pub archive: ArchiveState,
}

impl SessionCache {
    pub fn enforce_bounds(&mut self, live_limit: usize) {
        let limit = live_limit.max(1);
        trim_vec_front(&mut self.live_power_history, limit);
        trim_vec_front(&mut self.live_snapshots, limit);
        self.archive.enforce_bounds();
    }

    pub fn sanitize_power_data(&mut self, max_reasonable_power: f64) {
        for snapshot in &mut self.live_snapshots {
            snapshot
                .samples
                .retain(|sample| is_reasonable_power(sample.power, max_reasonable_power));
            for sample in &mut snapshot.samples {
                sample.power = sanitize_power(sample.power, max_reasonable_power);
            }
        }

        if !self.live_snapshots.is_empty() {
            self.live_power_history = self
                .live_snapshots
                .iter()
                .map(|snapshot| snapshot.samples.iter().map(|sample| sample.power).sum())
                .collect();
        } else {
            self.live_power_history
                .retain(|value| is_reasonable_power(*value, max_reasonable_power));
            for value in &mut self.live_power_history {
                *value = sanitize_power(*value, max_reasonable_power);
            }
        }

        self.archive.sanitize_power_data(max_reasonable_power);
    }
}

#[derive(Debug, Clone)]
pub struct LoadedSessionCache {
    pub cache: SessionCache,
    pub gap_millis: u64,
    pub hydrate_live: bool,
}

pub fn unix_time_secs_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn unix_time_millis_now() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_secs()
        .saturating_mul(1_000)
        .saturating_add(u64::from(now.subsec_millis()))
}

fn continuity_threshold_millis(refresh_every: Duration) -> u64 {
    refresh_every
        .as_millis()
        .saturating_mul(QUICK_HYDRATE_GAP_MULTIPLIER as u128)
        .min(u128::from(u64::MAX)) as u64
}

pub fn continuity_threshold_secs(refresh_every: Duration) -> u64 {
    let millis = continuity_threshold_millis(refresh_every);
    millis.saturating_add(999) / 1_000
}

pub fn should_hydrate_live(gap_millis: u64, refresh_every: Duration) -> bool {
    gap_millis <= continuity_threshold_millis(refresh_every)
}

pub fn cache_file_path() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;

    Ok(PathBuf::from(home)
        .join("Library")
        .join("Caches")
        .join("etop")
        .join(CACHE_FILE_NAME))
}

pub fn load_session_cache_for_startup(
    refresh_every: Duration,
    live_limit: usize,
) -> io::Result<Option<LoadedSessionCache>> {
    let path = cache_file_path()?;
    load_session_cache_for_startup_from_path(&path, refresh_every, live_limit)
}

pub fn save_session_cache(cache: &SessionCache) -> io::Result<()> {
    let path = cache_file_path()?;
    save_session_cache_to_path(&path, cache)
}

fn load_session_cache_for_startup_from_path(
    path: &Path,
    refresh_every: Duration,
    live_limit: usize,
) -> io::Result<Option<LoadedSessionCache>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };

    let mut reader = BufReader::new(file);
    let mut cache = decode_session_cache(&mut reader)?;
    cache.sanitize_power_data(MAX_REASONABLE_PERSISTED_POWER);
    cache.enforce_bounds(live_limit);

    let gap_millis = unix_time_millis_now().saturating_sub(cache.saved_at_unix_millis);

    Ok(Some(LoadedSessionCache {
        hydrate_live: should_hydrate_live(gap_millis, refresh_every),
        gap_millis,
        cache,
    }))
}

fn save_session_cache_to_path(path: &Path, cache: &SessionCache) -> io::Result<()> {
    let mut bounded = cache.clone();
    trim_vec_front(&mut bounded.live_power_history, MAX_LIVE_POWER_POINTS);
    trim_vec_front(&mut bounded.live_snapshots, MAX_LIVE_SNAPSHOTS);
    bounded.sanitize_power_data(MAX_REASONABLE_PERSISTED_POWER);
    bounded.archive.enforce_bounds();

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_path = path.with_extension(format!("tmp-{}-{nonce}", process::id()));

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)?;

    let mut writer = BufWriter::new(file);
    encode_session_cache(&bounded, &mut writer)?;
    writer.flush()?;

    let file = writer
        .into_inner()
        .map_err(|err| io::Error::other(err.to_string()))?;
    file.sync_all()?;
    drop(file);

    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn is_reasonable_power(value: f64, max_reasonable_power: f64) -> bool {
    value.is_finite() && (0.0..=max_reasonable_power).contains(&value)
}

fn sanitize_power(value: f64, max_reasonable_power: f64) -> f64 {
    if is_reasonable_power(value, max_reasonable_power) {
        value
    } else {
        0.0
    }
}

fn sanitize_tier_power(tier: &mut VecDeque<TierSample>, max_reasonable_power: f64) {
    tier.retain_mut(|sample| {
        if sample.sample_count == 0 {
            return false;
        }

        let avg_power = sample.total_power_sum / f64::from(sample.sample_count);
        if !is_reasonable_power(avg_power, max_reasonable_power) {
            return false;
        }

        sample.total_power_sum = avg_power * f64::from(sample.sample_count);
        sample.groups.retain(|group| {
            let avg_group_power = group.power_sum / f64::from(sample.sample_count);
            is_reasonable_power(avg_group_power, max_reasonable_power)
        });
        true
    });
}

fn normalize_group_sums<I>(grouped: I) -> Vec<GroupPowerSum>
where
    I: IntoIterator<Item = (String, f64)>,
{
    let mut sums: HashMap<String, f64> = HashMap::new();

    for (name, power) in grouped {
        *sums.entry(name).or_insert(0.0) += power;
    }

    let mut pairs: Vec<GroupPowerSum> = sums
        .into_iter()
        .map(|(name, power_sum)| GroupPowerSum { name, power_sum })
        .collect();
    pairs.sort_by(|a, b| a.name.cmp(&b.name));
    pairs
}

fn merge_group_sums(existing: &mut Vec<GroupPowerSum>, incoming: &[GroupPowerSum]) {
    let mut sums: HashMap<String, f64> = existing
        .iter()
        .map(|entry| (entry.name.clone(), entry.power_sum))
        .collect();

    for entry in incoming {
        *sums.entry(entry.name.clone()).or_insert(0.0) += entry.power_sum;
    }

    let mut merged: Vec<GroupPowerSum> = sums
        .into_iter()
        .map(|(name, power_sum)| GroupPowerSum { name, power_sum })
        .collect();
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    *existing = merged;
}

fn upsert_aggregate_bucket(
    tier: &mut VecDeque<TierSample>,
    bucket_secs: u64,
    capacity: usize,
    timestamp_secs: u64,
    total_power: f64,
    groups: &[GroupPowerSum],
    has_gap_from_previous: bool,
) {
    // A long downtime may produce a new sample that lands in the same wall-clock bucket
    // as the last persisted sample. In that case we intentionally append a second bucket
    // entry with the same bucket_start_secs instead of merging, so later archive consumers
    // can preserve the discontinuity instead of faking continuity across the gap.
    let bucket_start = if bucket_secs == 0 {
        timestamp_secs
    } else {
        timestamp_secs.saturating_sub(timestamp_secs % bucket_secs)
    };

    if !has_gap_from_previous {
        if let Some(last) = tier.back_mut() {
            if last.bucket_start_secs == bucket_start {
                last.sample_count = last.sample_count.saturating_add(1);
                last.total_power_sum += total_power;
                merge_group_sums(&mut last.groups, groups);
                return;
            }
        }
    }

    let gap_before = has_gap_from_previous && !tier.is_empty();
    tier.push_back(TierSample::from_single_sample(
        bucket_start,
        total_power,
        groups,
        gap_before,
    ));
    trim_to_capacity(tier, capacity);
}

fn trim_to_capacity<T>(deque: &mut VecDeque<T>, capacity: usize) {
    while deque.len() > capacity {
        let _ = deque.pop_front();
    }
}

fn trim_vec_front<T>(values: &mut Vec<T>, capacity: usize) {
    if values.len() <= capacity {
        return;
    }

    let drop = values.len() - capacity;
    values.drain(0..drop);
}

fn encode_session_cache<W: Write>(cache: &SessionCache, writer: &mut W) -> io::Result<()> {
    writer.write_all(CACHE_MAGIC)?;
    write_u16(writer, CACHE_VERSION)?;
    write_u64(writer, cache.saved_at_unix_millis)?;
    write_u64(writer, cache.last_tick)?;
    write_u64(writer, cache.archive.last_sample_unix_secs.unwrap_or(0))?;

    write_len(writer, cache.live_power_history.len())?;
    for value in &cache.live_power_history {
        write_f64(writer, *value)?;
    }

    write_len(writer, cache.live_snapshots.len())?;
    for snapshot in &cache.live_snapshots {
        write_u64(writer, snapshot.tick)?;
        write_len(writer, snapshot.samples.len())?;

        for sample in &snapshot.samples {
            write_i32(writer, sample.pid)?;
            write_string(writer, &sample.process)?;
            write_f64(writer, sample.power)?;
        }
    }

    encode_tier(writer, &cache.archive.raw_2s)?;
    encode_tier(writer, &cache.archive.agg_10s)?;
    encode_tier(writer, &cache.archive.agg_60s)?;

    Ok(())
}

fn encode_tier<W: Write>(writer: &mut W, tier: &VecDeque<TierSample>) -> io::Result<()> {
    write_len(writer, tier.len())?;

    for sample in tier {
        write_u64(writer, sample.bucket_start_secs)?;
        write_u32(writer, sample.sample_count)?;
        write_f64(writer, sample.total_power_sum)?;

        write_len(writer, sample.groups.len())?;
        for group in &sample.groups {
            write_string(writer, &group.name)?;
            write_f64(writer, group.power_sum)?;
        }
        write_bool(writer, sample.gap_before)?;
    }

    Ok(())
}

fn decode_session_cache<R: Read>(reader: &mut R) -> io::Result<SessionCache> {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != CACHE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cache magic",
        ));
    }

    let version = read_u16(reader)?;
    if version != CACHE_VERSION_V2 && version != CACHE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported cache version: {version}"),
        ));
    }

    let saved_at_unix_millis = read_u64(reader)?;
    let last_tick = read_u64(reader)?;
    let last_sample_unix_secs = match read_u64(reader)? {
        0 => None,
        value => Some(value),
    };

    let power_len = read_len(reader, MAX_LIVE_POWER_POINTS, "live power history")?;
    let mut live_power_history = Vec::with_capacity(power_len);
    for _ in 0..power_len {
        live_power_history.push(read_f64(reader)?);
    }

    let snapshot_len = read_len(reader, MAX_LIVE_SNAPSHOTS, "live snapshots")?;
    let mut live_snapshots = Vec::with_capacity(snapshot_len);
    for _ in 0..snapshot_len {
        let tick = read_u64(reader)?;
        let row_len = read_len(reader, MAX_SNAPSHOT_ROWS, "snapshot rows")?;
        let mut samples = Vec::with_capacity(row_len);

        for _ in 0..row_len {
            samples.push(LiveProcessSample {
                pid: read_i32(reader)?,
                process: read_string(reader, MAX_STRING_BYTES)?,
                power: read_f64(reader)?,
            });
        }

        live_snapshots.push(LiveSnapshot { tick, samples });
    }

    let mut archive = ArchiveState {
        raw_2s: decode_tier(reader, version)?,
        agg_10s: decode_tier(reader, version)?,
        agg_60s: decode_tier(reader, version)?,
        last_sample_unix_secs,
    };
    archive.enforce_bounds();

    Ok(SessionCache {
        saved_at_unix_millis,
        last_tick,
        live_power_history,
        live_snapshots,
        archive,
    })
}

fn decode_tier<R: Read>(reader: &mut R, version: u16) -> io::Result<VecDeque<TierSample>> {
    let len = read_len(reader, MAX_TIER_SAMPLES, "tier samples")?;
    let mut tier = VecDeque::with_capacity(len);

    for _ in 0..len {
        let bucket_start_secs = read_u64(reader)?;
        let sample_count = read_u32(reader)?;
        let total_power_sum = read_f64(reader)?;

        let groups_len = read_len(reader, MAX_GROUPS_PER_SAMPLE, "grouped samples")?;
        let mut groups = Vec::with_capacity(groups_len);
        for _ in 0..groups_len {
            groups.push(GroupPowerSum {
                name: read_string(reader, MAX_STRING_BYTES)?,
                power_sum: read_f64(reader)?,
            });
        }

        let gap_before = if version == CACHE_VERSION {
            read_bool(reader)?
        } else {
            false
        };

        tier.push_back(TierSample {
            bucket_start_secs,
            sample_count,
            total_power_sum,
            groups,
            gap_before,
        });
    }

    Ok(tier)
}

fn write_len<W: Write>(writer: &mut W, len: usize) -> io::Result<()> {
    let value = u32::try_from(len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "length exceeds u32"))?;
    write_u32(writer, value)
}

fn read_len<R: Read>(reader: &mut R, max: usize, label: &str) -> io::Result<usize> {
    let value = read_u32(reader)? as usize;
    if value > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} length {value} exceeds max {max}"),
        ));
    }

    Ok(value)
}

fn write_string<W: Write>(writer: &mut W, value: &str) -> io::Result<()> {
    write_len(writer, value.len())?;
    writer.write_all(value.as_bytes())
}

fn read_string<R: Read>(reader: &mut R, max_bytes: usize) -> io::Result<String> {
    let len = read_len(reader, max_bytes, "string")?;
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes)?;

    String::from_utf8(bytes)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("utf8 error: {err}")))
}

fn write_u16<W: Write>(writer: &mut W, value: u16) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u32<W: Write>(writer: &mut W, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u64<W: Write>(writer: &mut W, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_i32<W: Write>(writer: &mut W, value: i32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_f64<W: Write>(writer: &mut W, value: f64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_bool<W: Write>(writer: &mut W, value: bool) -> io::Result<()> {
    writer.write_all(&[u8::from(value)])
}

fn read_u16<R: Read>(reader: &mut R) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u32<R: Read>(reader: &mut R) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64<R: Read>(reader: &mut R) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_i32<R: Read>(reader: &mut R) -> io::Result<i32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(i32::from_le_bytes(buf))
}

fn read_f64<R: Read>(reader: &mut R) -> io::Result<f64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(f64::from_le_bytes(buf))
}

fn read_bool<R: Read>(reader: &mut R) -> io::Result<bool> {
    let mut buf = [0u8; 1];
    reader.read_exact(&mut buf)?;
    match buf[0] {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid bool value: {value}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_session_cache_v2<W: Write>(cache: &SessionCache, writer: &mut W) -> io::Result<()> {
        writer.write_all(CACHE_MAGIC)?;
        write_u16(writer, CACHE_VERSION_V2)?;
        write_u64(writer, cache.saved_at_unix_millis)?;
        write_u64(writer, cache.last_tick)?;
        write_u64(writer, cache.archive.last_sample_unix_secs.unwrap_or(0))?;

        write_len(writer, cache.live_power_history.len())?;
        for value in &cache.live_power_history {
            write_f64(writer, *value)?;
        }

        write_len(writer, cache.live_snapshots.len())?;
        for snapshot in &cache.live_snapshots {
            write_u64(writer, snapshot.tick)?;
            write_len(writer, snapshot.samples.len())?;

            for sample in &snapshot.samples {
                write_i32(writer, sample.pid)?;
                write_string(writer, &sample.process)?;
                write_f64(writer, sample.power)?;
            }
        }

        encode_tier_v2(writer, &cache.archive.raw_2s)?;
        encode_tier_v2(writer, &cache.archive.agg_10s)?;
        encode_tier_v2(writer, &cache.archive.agg_60s)?;

        Ok(())
    }

    fn encode_tier_v2<W: Write>(writer: &mut W, tier: &VecDeque<TierSample>) -> io::Result<()> {
        write_len(writer, tier.len())?;

        for sample in tier {
            write_u64(writer, sample.bucket_start_secs)?;
            write_u32(writer, sample.sample_count)?;
            write_f64(writer, sample.total_power_sum)?;

            write_len(writer, sample.groups.len())?;
            for group in &sample.groups {
                write_string(writer, &group.name)?;
                write_f64(writer, group.power_sum)?;
            }
        }

        Ok(())
    }

    #[test]
    fn session_cache_roundtrips_binary_format() {
        let mut archive = ArchiveState::default();
        archive.record_sample(
            100,
            6,
            10.0,
            [("Safari".to_string(), 6.0), ("Mail".to_string(), 4.0)],
        );
        archive.record_sample(
            102,
            6,
            8.0,
            [("Safari".to_string(), 5.0), ("Slack".to_string(), 3.0)],
        );

        let cache = SessionCache {
            saved_at_unix_millis: 123_456,
            last_tick: 42,
            live_power_history: vec![1.0, 2.5, 3.75],
            live_snapshots: vec![
                LiveSnapshot {
                    tick: 41,
                    samples: vec![
                        LiveProcessSample {
                            pid: 1,
                            process: "Safari".to_string(),
                            power: 4.0,
                        },
                        LiveProcessSample {
                            pid: 2,
                            process: "Mail".to_string(),
                            power: 1.0,
                        },
                    ],
                },
                LiveSnapshot {
                    tick: 42,
                    samples: vec![LiveProcessSample {
                        pid: 1,
                        process: "Safari".to_string(),
                        power: 5.0,
                    }],
                },
            ],
            archive,
        };

        let mut bytes = Vec::new();
        encode_session_cache(&cache, &mut bytes).expect("encode should succeed");

        let decoded = decode_session_cache(&mut bytes.as_slice()).expect("decode should succeed");
        assert_eq!(decoded, cache);
    }

    #[test]
    fn session_cache_decodes_v2_binary_format_with_gap_flags_defaulted() {
        let cache = SessionCache {
            saved_at_unix_millis: 123_456,
            last_tick: 42,
            live_power_history: vec![1.0, 2.5],
            live_snapshots: vec![LiveSnapshot {
                tick: 42,
                samples: vec![LiveProcessSample {
                    pid: 1,
                    process: "Safari".to_string(),
                    power: 5.0,
                }],
            }],
            archive: ArchiveState {
                raw_2s: VecDeque::from(vec![
                    TierSample {
                        bucket_start_secs: 100,
                        sample_count: 1,
                        total_power_sum: 10.0,
                        groups: vec![GroupPowerSum {
                            name: "Safari".to_string(),
                            power_sum: 10.0,
                        }],
                        gap_before: false,
                    },
                    TierSample {
                        bucket_start_secs: 108,
                        sample_count: 1,
                        total_power_sum: 12.0,
                        groups: vec![GroupPowerSum {
                            name: "Safari".to_string(),
                            power_sum: 12.0,
                        }],
                        gap_before: false,
                    },
                ]),
                last_sample_unix_secs: Some(108),
                ..ArchiveState::default()
            },
        };

        let mut bytes = Vec::new();
        encode_session_cache_v2(&cache, &mut bytes).expect("v2 encode should succeed");

        let decoded = decode_session_cache(&mut bytes.as_slice()).expect("decode should succeed");
        assert_eq!(decoded, cache);
    }

    #[test]
    fn archive_tiers_stay_bounded() {
        let mut archive = ArchiveState::default();

        for idx in 0..25_000_u64 {
            archive.record_sample(
                idx * 2,
                6,
                (idx % 100) as f64,
                [("Safari".to_string(), (idx % 10) as f64)],
            );
        }

        assert_eq!(archive.raw_2s.len(), RAW_2S_CAPACITY);
        assert_eq!(archive.agg_10s.len(), AGG_10S_CAPACITY);
        assert_eq!(archive.agg_60s.len(), AGG_60S_CAPACITY);

        assert!(
            archive
                .raw_2s
                .front()
                .map(|sample| sample.bucket_start_secs)
                .unwrap_or(0)
                > 0
        );
        assert!(
            archive
                .agg_10s
                .front()
                .map(|sample| sample.bucket_start_secs)
                .unwrap_or(0)
                > 0
        );
        assert!(
            archive
                .agg_60s
                .front()
                .map(|sample| sample.bucket_start_secs)
                .unwrap_or(0)
                > 0
        );
    }

    #[test]
    fn archive_does_not_merge_same_10s_bucket_after_long_gap() {
        let mut archive = ArchiveState::default();
        let threshold = continuity_threshold_secs(Duration::from_secs(2));

        archive.record_sample(100, threshold, 10.0, [("Safari".to_string(), 10.0)]);
        archive.record_sample(108, threshold, 12.0, [("Safari".to_string(), 12.0)]);

        assert_eq!(archive.agg_10s.len(), 2);
        assert_eq!(archive.agg_10s[0].bucket_start_secs, 100);
        assert_eq!(archive.agg_10s[1].bucket_start_secs, 100);
        assert_eq!(archive.agg_10s[0].sample_count, 1);
        assert_eq!(archive.agg_10s[1].sample_count, 1);
        assert!(!archive.agg_10s[0].gap_before);
        assert!(archive.agg_10s[1].gap_before);
    }

    #[test]
    fn archive_does_not_merge_same_60s_bucket_after_long_gap() {
        let mut archive = ArchiveState::default();
        let threshold = continuity_threshold_secs(Duration::from_secs(2));

        archive.record_sample(124, threshold, 10.0, [("Safari".to_string(), 10.0)]);
        archive.record_sample(136, threshold, 12.0, [("Safari".to_string(), 12.0)]);

        assert_eq!(archive.agg_60s.len(), 2);
        assert_eq!(archive.agg_60s[0].bucket_start_secs, 120);
        assert_eq!(archive.agg_60s[1].bucket_start_secs, 120);
        assert_eq!(archive.agg_60s[0].sample_count, 1);
        assert_eq!(archive.agg_60s[1].sample_count, 1);
        assert!(!archive.agg_60s[0].gap_before);
        assert!(archive.agg_60s[1].gap_before);
    }

    #[test]
    fn archive_marks_gap_before_adjacent_bucket_after_long_gap() {
        let mut archive = ArchiveState::default();
        let threshold = continuity_threshold_secs(Duration::from_secs(2));

        archive.record_sample(100, threshold, 10.0, [("Safari".to_string(), 10.0)]);
        archive.record_sample(115, threshold, 12.0, [("Safari".to_string(), 12.0)]);

        assert_eq!(archive.agg_10s.len(), 2);
        assert_eq!(archive.agg_10s[0].bucket_start_secs, 100);
        assert_eq!(archive.agg_10s[1].bucket_start_secs, 110);
        assert!(!archive.agg_10s[0].gap_before);
        assert!(archive.agg_10s[1].gap_before);
    }

    #[test]
    fn session_cache_sanitizes_implausible_live_and_archive_power_values() {
        let mut cache = SessionCache {
            saved_at_unix_millis: 123,
            last_tick: 2,
            live_power_history: vec![10.0, 55_443_352.6],
            live_snapshots: vec![
                LiveSnapshot {
                    tick: 1,
                    samples: vec![LiveProcessSample {
                        pid: 10,
                        process: "Safari".to_string(),
                        power: 10.0,
                    }],
                },
                LiveSnapshot {
                    tick: 2,
                    samples: vec![
                        LiveProcessSample {
                            pid: 336,
                            process: "powerd".to_string(),
                            power: 55_443_258.0,
                        },
                        LiveProcessSample {
                            pid: 20,
                            process: "Mail".to_string(),
                            power: 4.6,
                        },
                    ],
                },
            ],
            archive: ArchiveState {
                raw_2s: VecDeque::from(vec![
                    TierSample {
                        bucket_start_secs: 100,
                        sample_count: 1,
                        total_power_sum: 10.0,
                        groups: Vec::new(),
                        gap_before: false,
                    },
                    TierSample {
                        bucket_start_secs: 102,
                        sample_count: 1,
                        total_power_sum: 55_443_352.6,
                        groups: Vec::new(),
                        gap_before: false,
                    },
                ]),
                ..ArchiveState::default()
            },
        };

        cache.sanitize_power_data(MAX_REASONABLE_PERSISTED_POWER);

        assert_eq!(cache.live_power_history, vec![10.0, 4.6]);
        assert_eq!(cache.live_snapshots[1].samples.len(), 1);
        assert_eq!(cache.live_snapshots[1].samples[0].process, "Mail");
        assert_eq!(cache.archive.raw_2s.len(), 1);
        assert_eq!(cache.archive.raw_2s[0].total_power_sum, 10.0);
    }

    #[test]
    fn quick_gap_hydration_gate_respects_threshold() {
        let refresh = Duration::from_secs(2);
        let threshold = refresh.as_millis() as u64 * QUICK_HYDRATE_GAP_MULTIPLIER;

        assert!(should_hydrate_live(0, refresh));
        assert!(should_hydrate_live(threshold, refresh));
        assert!(!should_hydrate_live(threshold + 1, refresh));
    }
}

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use sha2::{Digest, Sha256};
use tokio::{fs, sync::RwLock};

pub const ISLANDS_FILE: &str = "islands.txt";
/// islands.txt is archived at most daily, so it doesn't define timeline points; each point
/// uses whichever islands copy is closest instead.
const TIMELINE_FILES: [&str; 3] = ["players.txt", "alliances.txt", "towns.txt"];
/// Only the islands' town-count column changes between regenerations.
const ISLANDS_MIN_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// An upstream node writes all of its files within a second or two of each other.
const CLUSTER_SECS: u64 = 120;
const LISTING_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// upstream `Last-Modified`, in unix seconds
    pub lm: u64,
    pub hash: String,
    pub path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecordOutcome {
    Stored,
    Duplicate,
    Throttled,
}

/// Every distinct version of every data file, stored as
/// `{dir}/{server}/{file}/{lm}-{hash}.txt.gz`.
pub struct Archive {
    dir: PathBuf,
    listings: RwLock<HashMap<(String, String), (Instant, Vec<Version>)>>,
}

impl Archive {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            listings: RwLock::new(HashMap::new()),
        }
    }

    fn file_dir(&self, server: &str, file: &str) -> PathBuf {
        self.dir.join(server).join(file)
    }

    /// All versions of a file, oldest first. Listings are cached briefly because the
    /// directory is a Cloud Storage FUSE mount, where listing is slow.
    pub async fn versions(&self, server: &str, file: &str) -> Vec<Version> {
        let key = (server.to_owned(), file.to_owned());
        if let Some((listed_at, versions)) = self.listings.read().await.get(&key) {
            if listed_at.elapsed() < LISTING_TTL {
                return versions.clone();
            }
        }
        let versions = list_versions(&self.file_dir(server, file)).await;
        self.listings
            .write()
            .await
            .insert(key, (Instant::now(), versions.clone()));
        versions
    }

    pub async fn record(
        &self,
        server: &str,
        file: &str,
        lm: u64,
        data: &[u8],
    ) -> std::io::Result<RecordOutcome> {
        let hash = canonical_hash(data);
        let existing = self.versions(server, file).await;
        if existing.iter().any(|v| v.hash == hash) {
            return Ok(RecordOutcome::Duplicate);
        }
        if file == ISLANDS_FILE
            && existing
                .last()
                .is_some_and(|newest| lm.saturating_sub(newest.lm) < ISLANDS_MIN_INTERVAL_SECS)
        {
            return Ok(RecordOutcome::Throttled);
        }

        let dir = self.file_dir(server, file);
        fs::create_dir_all(&dir).await?;
        let name = format!("{lm}-{hash}.txt.gz");
        let path = dir.join(&name);
        // Write under a name `list_versions` ignores, so a half-written object is never read.
        let tmp = dir.join(format!(".{name}.tmp"));
        fs::write(&tmp, gzip(data)?).await?;
        fs::rename(&tmp, &path).await?;

        let mut updated = existing;
        updated.push(Version { lm, hash, path });
        self.listings.write().await.insert(
            (server.to_owned(), file.to_owned()),
            (Instant::now(), normalize(updated)),
        );
        Ok(RecordOutcome::Stored)
    }

    pub async fn newest(&self, server: &str, file: &str) -> Option<Version> {
        self.versions(server, file).await.pop()
    }

    pub async fn at(&self, server: &str, file: &str, t: u64) -> Option<Version> {
        resolve_at(&self.versions(server, file).await, file, t).cloned()
    }

    pub async fn read_gz(&self, version: &Version) -> std::io::Result<Vec<u8>> {
        fs::read(&version.path).await
    }

    pub async fn read_plain(&self, version: &Version) -> std::io::Result<Bytes> {
        let gz = self.read_gz(version).await?;
        let mut plain = Vec::new();
        GzDecoder::new(gz.as_slice()).read_to_end(&mut plain)?;
        Ok(Bytes::from(plain))
    }

    pub async fn timeline(&self, server: &str) -> Vec<u64> {
        let mut per_file = Vec::with_capacity(TIMELINE_FILES.len());
        for file in TIMELINE_FILES {
            per_file.push(self.versions(server, file).await);
        }
        let has_islands = !self.versions(server, ISLANDS_FILE).await.is_empty();
        timeline_points(&per_file, has_islands)
    }
}

/// SHA-256 over the rows in sorted order: the upstream nodes serve the same rows in
/// different orders, which must not count as a new version.
pub fn canonical_hash(data: &[u8]) -> String {
    let mut rows: Vec<&[u8]> = data
        .split(|&b| b == b'\n')
        .map(|row| row.strip_suffix(b"\r").unwrap_or(row))
        .filter(|row| !row.is_empty())
        .collect();
    rows.sort_unstable();
    let mut hasher = Sha256::new();
    for row in rows {
        hasher.update(row);
        hasher.update(b"\n");
    }
    hasher.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

fn parse_name(name: &str) -> Option<(u64, String)> {
    let (lm, hash) = name.strip_suffix(".txt.gz")?.split_once('-')?;
    if hash.len() != 16 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((lm.parse().ok()?, hash.to_owned()))
}

async fn list_versions(dir: &Path) -> Vec<Version> {
    let mut versions = Vec::new();
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return versions;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Some((lm, hash)) = entry.file_name().to_str().and_then(parse_name) {
            versions.push(Version {
                lm,
                hash,
                path: entry.path(),
            });
        }
    }
    normalize(versions)
}

/// Sort oldest first and collapse same-content versions (e.g. from two instances racing, or
/// the same data served by two nodes) into the earliest one.
fn normalize(mut versions: Vec<Version>) -> Vec<Version> {
    versions.sort_by(|a, b| a.lm.cmp(&b.lm).then_with(|| a.hash.cmp(&b.hash)));
    let mut seen = HashSet::new();
    versions.retain(|v| seen.insert(v.hash.clone()));
    versions
}

fn resolve_at<'a>(versions: &'a [Version], file: &str, t: u64) -> Option<&'a Version> {
    let at_or_before = versions.iter().rev().find(|v| v.lm <= t);
    if file == ISLANDS_FILE {
        at_or_before.or_else(|| versions.first())
    } else {
        at_or_before
    }
}

fn timeline_points(per_file: &[Vec<Version>], has_islands: bool) -> Vec<u64> {
    if !has_islands {
        return Vec::new();
    }
    let mut lms: Vec<u64> = per_file.iter().flatten().map(|v| v.lm).collect();
    lms.sort_unstable();

    let mut points: Vec<u64> = Vec::new();
    let mut cluster_start = None;
    for lm in lms {
        match cluster_start {
            Some(start) if lm - start <= CLUSTER_SECS => *points.last_mut().unwrap() = lm,
            _ => {
                cluster_start = Some(lm);
                points.push(lm);
            }
        }
    }
    points.retain(|&p| per_file.iter().all(|versions| versions.iter().any(|v| v.lm <= p)));
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(lm: u64, hash: &str) -> Version {
        Version {
            lm,
            hash: hash.to_owned(),
            path: PathBuf::new(),
        }
    }

    // 2026-09-11 05:13:01 / 05:13:02 / 05:22:01 UTC, as served by the two us145 nodes
    const T0513_01: u64 = 1_789_103_581;
    const T0513_02: u64 = 1_789_103_582;
    const T0522_01: u64 = 1_789_104_121;

    #[test]
    fn hash_ignores_row_order() {
        assert_eq!(
            canonical_hash(b"1873947,TomOmbre,202,330,304,1\n848978981,Ramon,,441,302,1\n"),
            canonical_hash(b"848978981,Ramon,,441,302,1\n1873947,TomOmbre,202,330,304,1")
        );
    }

    #[test]
    fn hash_detects_changed_row() {
        assert_ne!(
            canonical_hash(b"1330,848965218,55.The+Run,596,506,0,8631\n"),
            canonical_hash(b"1330,848965218,55.The+Run,596,506,0,8665\n")
        );
    }

    #[test]
    fn parses_only_well_formed_names() {
        assert_eq!(
            parse_name("1789103581-0123456789abcdef.txt.gz"),
            Some((T0513_01, "0123456789abcdef".to_owned()))
        );
        assert_eq!(parse_name(".1789103581-0123456789abcdef.txt.gz.tmp"), None);
        assert_eq!(parse_name("1789103581-short.txt.gz"), None);
        assert_eq!(parse_name("notanumber-0123456789abcdef.txt.gz"), None);
    }

    #[test]
    fn normalize_keeps_earliest_of_duplicate_content() {
        let versions = normalize(vec![v(T0522_01, "aa"), v(T0513_01, "aa"), v(T0513_02, "bb")]);
        assert_eq!(versions, vec![v(T0513_01, "aa"), v(T0513_02, "bb")]);
    }

    #[test]
    fn timeline_clusters_one_nodes_files_into_one_point() {
        let players = vec![v(T0513_01, "p1")];
        let alliances = vec![v(T0513_01, "a1"), v(T0522_01, "a2")];
        let towns = vec![v(T0513_02, "t1"), v(T0522_01, "t2")];
        assert_eq!(
            timeline_points(&[players, alliances, towns], true),
            vec![T0513_02, T0522_01]
        );
    }

    #[test]
    fn timeline_needs_every_file_and_islands() {
        let players = vec![v(T0522_01, "p1")];
        let alliances = vec![v(T0513_01, "a1")];
        let towns = vec![v(T0513_02, "t1")];
        let per_file = [players, alliances, towns];
        assert_eq!(timeline_points(&per_file, true), vec![T0522_01]);
        assert!(timeline_points(&per_file, false).is_empty());
    }

    #[test]
    fn at_resolves_newest_version_not_after_t() {
        let towns = vec![v(T0513_02, "t1"), v(T0522_01, "t2")];
        assert_eq!(resolve_at(&towns, "towns.txt", T0522_01 - 1).unwrap().hash, "t1");
        assert_eq!(resolve_at(&towns, "towns.txt", T0522_01).unwrap().hash, "t2");
        assert_eq!(resolve_at(&towns, "towns.txt", T0513_01), None);
        let islands = vec![v(T0522_01, "i1")];
        assert_eq!(resolve_at(&islands, ISLANDS_FILE, T0513_01).unwrap().hash, "i1");
    }

    #[tokio::test]
    async fn record_dedupes_throttles_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("reflector-archive-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let archive = Archive::new(dir.clone());

        let first = b"1,a,0\n2,b,0\n";
        let reordered = b"2,b,0\n1,a,0\n";
        let changed = b"1,a,0\n2,b,5\n";
        assert_eq!(archive.record("us145", "towns.txt", T0513_02, first).await.unwrap(), RecordOutcome::Stored);
        assert_eq!(archive.record("us145", "towns.txt", T0522_01, reordered).await.unwrap(), RecordOutcome::Duplicate);
        assert_eq!(archive.record("us145", "towns.txt", T0522_01, changed).await.unwrap(), RecordOutcome::Stored);

        let newest = archive.newest("us145", "towns.txt").await.unwrap();
        assert_eq!(newest.lm, T0522_01);
        assert_eq!(archive.read_plain(&newest).await.unwrap().as_ref(), changed);
        let older = archive.at("us145", "towns.txt", T0522_01 - 1).await.unwrap();
        assert_eq!(archive.read_plain(&older).await.unwrap().as_ref(), first);

        let day = ISLANDS_MIN_INTERVAL_SECS;
        assert_eq!(archive.record("us145", ISLANDS_FILE, T0513_02, b"1,0,18,1,20,iron,wood\n").await.unwrap(), RecordOutcome::Stored);
        assert_eq!(archive.record("us145", ISLANDS_FILE, T0513_02 + 3600, b"1,0,18,1,19,iron,wood\n").await.unwrap(), RecordOutcome::Throttled);
        assert_eq!(archive.record("us145", ISLANDS_FILE, T0513_02 + day, b"1,0,18,1,19,iron,wood\n").await.unwrap(), RecordOutcome::Stored);

        // a fresh Archive (e.g. another instance) sees the same versions on disk
        let other = Archive::new(dir.clone());
        assert_eq!(other.versions("us145", "towns.txt").await.len(), 2);
        assert_eq!(other.versions("us145", ISLANDS_FILE).await.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

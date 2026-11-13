//! `cortex update` on Windows: download the release, check it and swap the binary, all in-process.
//! Starting PowerShell with a download-and-run command (`irm … | iex`) from an unsigned program is
//! what malware does, so Microsoft Defender blocks it (as Trojan:Win32/Commando).

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// The release asset with the Windows build: a zip with cortex.exe at its top level.
pub const WINDOWS_ASSET: &str = "cortex-x86_64-pc-windows-msvc.zip";

/// Install release `tag` of `repo` as `dir`\cortex.exe. Returns what the new binary says its
/// version is. Windows can't overwrite a running exe but can rename it, so this works while
/// Cortex is running (the old one is left as cortex.exe.old and replaced on the next update).
#[cfg(windows)]
pub fn install_windows(repo: &str, tag: &str, dir: &Path, say: impl Fn(&str)) -> Result<String, String> {
    let rel = format!("https://github.com/{repo}/releases/download/{tag}");
    say(&format!("Downloading {rel}/{WINDOWS_ASSET}"));
    let zip = download(&format!("{rel}/{WINDOWS_ASSET}"))?;
    let sums = download(&format!("{rel}/SHA256SUMS.txt")).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
    verify(&zip, sums.as_deref())?;
    say("Checksum OK");
    let exe_bytes = extract(&zip, "cortex.exe")?;
    let exe = dir.join("cortex.exe");
    let new = dir.join("cortex.exe.new");
    let old = dir.join("cortex.exe.old");
    std::fs::write(&new, exe_bytes).map_err(|e| format!("couldn't write {}: {e}", new.display()))?;
    let _ = std::fs::remove_file(&old); // left by the last update
    if exe.exists() {
        std::fs::rename(&exe, &old).map_err(|e| { let _ = std::fs::remove_file(&new); format!("couldn't move the old cortex.exe aside: {e}") })?;
    }
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe); // put the old one back
        return Err(format!("couldn't put the new cortex.exe in place: {e}"));
    }
    let out = std::process::Command::new(&exe).arg("--version").output()
        .map_err(|e| format!("the new cortex.exe didn't run: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(windows)]
fn download(url: &str) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url).set("User-Agent", concat!("cortex/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120)).call()
        .map_err(|e| format!("couldn't download {url}: {e}"))?;
    let mut body = vec![];
    // the release zip is a few MB; anything far bigger isn't it
    resp.into_reader().take(200_000_000).read_to_end(&mut body).map_err(|e| format!("download of {url} broke off: {e}"))?;
    Ok(body)
}

/// Check the downloaded release zip against SHA256SUMS.txt (`sums`, None if it couldn't be
/// downloaded). Every release publishes one, so a missing checksum fails the update too: the
/// download can't be told apart from a swapped one.
pub fn verify(zip: &[u8], sums: Option<&str>) -> Result<(), String> {
    match sums.and_then(|s| expected_sha256(s, WINDOWS_ASSET)) {
        Some(expected) if expected == sha256_hex(zip) => Ok(()),
        Some(_) => Err(format!("checksum mismatch for {WINDOWS_ASSET}: the download may be corrupted or tampered with. Cortex was not updated.")),
        None => Err(format!("couldn't get the checksum for {WINDOWS_ASSET} (SHA256SUMS.txt), so the download can't be verified. Cortex was not updated; try again later.")),
    }
}

/// The SHA-256 that SHA256SUMS.txt lists for `file` ("<hash>  <file>", or "*<file>" in binary mode).
pub fn expected_sha256(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, name) = l.trim().split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == file).then(|| hash.to_lowercase())
    })
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// The contents of `name` in a zip archive.
#[cfg(windows)]
pub fn extract(zip: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).map_err(|e| format!("the download isn't a valid zip: {e}"))?;
    let mut file = archive.by_name(name).map_err(|_| format!("{name} isn't in the download"))?;
    let mut out = vec![];
    file.read_to_end(&mut out).map_err(|e| format!("couldn't unpack {name}: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_checksum_for_the_asset() {
        let sums = "0aa1  cortex-aarch64-apple-darwin.tar.gz\nABCD12 *cortex-x86_64-pc-windows-msvc.zip\n";
        assert_eq!(expected_sha256(sums, WINDOWS_ASSET).as_deref(), Some("abcd12"));
        assert_eq!(expected_sha256(sums, "cortex-x86_64-unknown-linux-gnu.tar.gz"), None);
    }

    #[test]
    fn an_update_installs_only_a_download_that_matches_its_checksum() {
        let zip = b"the release zip";
        let sums = |hash: &str| format!("{hash}  {WINDOWS_ASSET}\n0aa1  cortex-aarch64-apple-darwin.tar.gz\n");
        assert!(verify(zip, Some(&sums(&sha256_hex(zip)))).is_ok());
        assert!(verify(zip, Some(&sums("0bad"))).unwrap_err().contains("mismatch"));
        // every release publishes a checksum: without one, nothing says the download is the release
        assert!(verify(zip, None).is_err(), "the checksum file didn't download");
        assert!(verify(zip, Some("0aa1  cortex-aarch64-apple-darwin.tar.gz\n")).is_err(), "it doesn't list the asset");
    }

    #[test]
    fn hashes_like_sha256sum() {
        // the standard test vector for "abc"
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[cfg(windows)]
    #[test]
    fn unpacks_cortex_exe_from_a_release_zip() {
        // laid out like the release zip: cortex.exe, LICENSE and README.md at the top, deflated
        use std::io::Write;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in [("cortex.exe", "not really an exe"), ("LICENSE", "license"), ("README.md", "readme")] {
            w.start_file(name, opts).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        let zip = w.finish().unwrap().into_inner();
        assert_eq!(extract(&zip, "cortex.exe").unwrap(), b"not really an exe");
        assert!(extract(&zip, "missing.exe").is_err());
        assert!(extract(b"not a zip", "cortex.exe").is_err());
    }

    /// Installs a real release from GitHub into a temporary folder. Needs the network, so it only
    /// runs on request: `cargo test -p cortex self_update -- --ignored`
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn installs_a_real_release() {
        let dir = std::env::temp_dir().join(format!("cortex_update{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cortex.exe"), "the old one").unwrap();
        let version = install_windows("bmap02/Cortex", "v0.1.3", &dir, |s| println!("{s}")).unwrap();
        assert_eq!(version, "cortex 0.1.3");
        assert_eq!(std::fs::read(dir.join("cortex.exe.old")).unwrap(), b"the old one");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

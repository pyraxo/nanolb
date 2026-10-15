use sha2::Digest;

pub(crate) fn sha256_hex(s: &str) -> String {
    hex::encode(sha2::Sha256::digest(s.as_bytes()))
}

#[expect(clippy::expect_used, reason = "secrets need the OS random source and there is no safe fallback")]
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("os rng");
    hex::encode(b)
}

pub(crate) fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

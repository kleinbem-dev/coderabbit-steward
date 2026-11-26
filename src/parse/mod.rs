pub mod check_run;
pub mod comment;
pub mod review;

use jiff::Timestamp;

pub const CODERABBIT_LOGINS: &[&str] = &["coderabbitai[bot]", "coderabbitai"];

pub fn is_coderabbit_login(login: &str) -> bool {
    CODERABBIT_LOGINS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(login))
}

pub fn parse_timestamp(raw: &str) -> Option<Timestamp> {
    raw.parse::<Timestamp>().ok()
}

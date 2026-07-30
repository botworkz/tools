pub(crate) const SECRET_MASK: &str = "***";

pub(crate) fn display_string_for_secret(raw: &str, is_secret: bool) -> String {
    if is_secret {
        SECRET_MASK.to_string()
    } else {
        raw.to_string()
    }
}

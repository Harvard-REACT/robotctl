//! `robotctl status --json`.
//!
//! The document's shape comes from [`Status`]'s own field order and `Serialize` derives — see
//! `mod.rs`. There is deliberately nothing to render here beyond calling serde, so the schema
//! cannot drift away from the model that produces it.
//!
//! Compact rather than pretty-printed: this is meant to be piped (`robotctl status --json | jq`)

use anyhow::{Context, Result};

use super::Status;

pub fn render(status: &Status) -> Result<String> {
    serde_json::to_string(status).context("serializing status as JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses `rendered` well enough to assert on it, without pulling in a JSON *parser*
    /// dependency for the test suite. Only used on output we just produced.
    fn contains_all(rendered: &str, fragments: &[&str]) {
        for fragment in fragments {
            assert!(
                rendered.contains(fragment),
                "expected {fragment} in {rendered}"
            );
        }
    }

    #[test]
    fn renders_the_documented_schema() {
        let rendered = render(&super::super::tests::example_status()).unwrap();

        assert!(rendered.starts_with('{') && rendered.ends_with('}'));

        // Spot-check the shape rather than the whole string, which would fail on every field
        // addition without catching anything. The grouping is the part worth pinning: it is what
        // consumers index into.
        contains_all(
            &rendered,
            &[
                r#""identity":{"robot_id":"gopigo-07""#,
                r#""ros_domain_id":7"#,
                r#""rauc":{"booted_slot":"A""#,
                r#""slot_states":{"A":"good","B":"good"}"#,
                r#""uptime_seconds":90000"#,
                r#""total_bytes":4294967296"#,
                r#""addresses":["192.168.1.50/24"]"#,
                r#""units":{"robotctl-wifi.service":"active"}"#,
                r#""failed_units":["broken.service"]"#,
            ],
        );
    }

    #[test]
    fn ordered_pairs_serialize_as_objects_in_order() {
        // `image` is a Vec of pairs so the build's key order survives; it still has to come out
        // as a JSON object, and in that order.
        let rendered = render(&super::super::tests::example_status()).unwrap();

        contains_all(
            &rendered,
            &[r#""image":{"image_basename":"gopigo-image","distro_version":"0.1-dev"}"#],
        );
    }

    #[test]
    fn absent_probes_render_as_null_not_missing_keys() {
        // Consumers should be able to rely on every documented key existing, so that "we did not
        // look" and "there is nothing there" are not the same as "the key is gone".
        let mut status = super::super::tests::example_status();
        status.system.kernel = None;
        status.system.uptime_seconds = None;
        status.rauc.booted_slot = None;

        let rendered = render(&status).unwrap();

        contains_all(
            &rendered,
            &[
                r#""kernel":null"#,
                r#""uptime_seconds":null"#,
                r#""booted_slot":null"#,
            ],
        );
    }

    #[test]
    fn escaping_is_serdes_problem_now() {
        // The hand-rolled writer this replaced had its own escaping tests. Keep one case so a
        // future change of serializer cannot silently reintroduce the bug they guarded against.
        let mut status = super::super::tests::example_status();
        status.identity.robot_id = "quote\"newline\nbell\u{7}".to_string();

        let rendered = render(&status).unwrap();

        contains_all(&rendered, &[r#""robot_id":"quote\"newline\nbell\u0007""#]);
    }
}

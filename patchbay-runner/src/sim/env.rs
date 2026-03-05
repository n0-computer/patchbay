use std::collections::HashMap;

use anyhow::{anyhow, Result};

/// Runtime environment for a sim: `NETSIM_*` vars, binary paths, and captures.
pub struct SimEnv {
    /// `NETSIM_IP_*`, `NETSIM_NS_*` vars derived from the lab.
    lab_vars: HashMap<String, String>,
    /// Named binary paths, keyed by binary `name` field.
    binaries: HashMap<String, String>,
    /// Captured values from prior spawn steps: `"step_id.name"` → value.
    captures: HashMap<String, String>,
}

impl SimEnv {
    /// Creates a new environment from lab-derived variables and resolved binary paths.
    pub fn new(lab_vars: HashMap<String, String>, binaries: HashMap<String, String>) -> Self {
        Self {
            lab_vars,
            binaries,
            captures: HashMap::new(),
        }
    }

    /// Return an iterator over `NETSIM_*` environment variables.
    pub fn process_env(&self) -> impl Iterator<Item = (&str, &str)> {
        self.lab_vars.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Interpolate a single string.
    ///
    /// Supports:
    /// - `${binary.<name>}` — path to the named binary
    /// - `${<step_id>.<capture>}` — captured value from a prior spawn
    /// - `$NETSIM_*` — lab variable (dollar-sign without braces)
    pub fn interpolate_str(&self, s: &str) -> Result<String> {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while !rest.is_empty() {
            if let Some(idx) = rest.find("${") {
                out.push_str(&rest[..idx]);
                rest = &rest[idx + 2..];
                let end = rest
                    .find('}')
                    .ok_or_else(|| anyhow!("unclosed '{{' in {:?}", s))?;
                let key = &rest[..end];
                rest = &rest[end + 1..];
                let val = self.resolve_brace(key)?;
                out.push_str(&val);
            } else if let Some(idx) = rest.find('$') {
                out.push_str(&rest[..idx]);
                rest = &rest[idx + 1..];
                // Read a simple identifier (A-Z, 0-9, _).
                let end = rest
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .unwrap_or(rest.len());
                let key = &rest[..end];
                rest = &rest[end..];
                if let Some(val) = self.lab_vars.get(key) {
                    out.push_str(val);
                } else if let Some(val) = self.lab_vars.get(&key.to_ascii_uppercase()) {
                    // Accept lowercase/mixed-case NETSIM vars in sim files.
                    out.push_str(val);
                } else {
                    // Unknown $VAR — leave as-is for the process environment.
                    out.push('$');
                    out.push_str(key);
                }
            } else {
                out.push_str(rest);
                break;
            }
        }
        Ok(out)
    }

    /// Resolve a `${key}` reference.
    fn resolve_brace(&self, key: &str) -> Result<String> {
        // ${binary.<name>}
        if let Some(bin_name) = key.strip_prefix("binary.") {
            return self
                .binaries
                .get(bin_name)
                .cloned()
                .ok_or_else(|| anyhow!("unknown binary '${{{key}}}' — check [[binary]] entries"));
        }
        // Captures: ${step_id.capture_name}
        if let Some(val) = self.captures.get(key) {
            return Ok(val.clone());
        }
        // Lab vars (unlikely to be brace-escaped, but support it anyway)
        if let Some(val) = self.lab_vars.get(key) {
            return Ok(val.clone());
        }
        Err(anyhow!("unknown variable '${{{key}}}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl SimEnv {
        fn set_capture(&mut self, step_id: &str, name: &str, value: String) {
            self.captures.insert(format!("{}.{}", step_id, name), value);
        }
    }

    #[test]
    fn interpolate_binary_and_capture_and_lab_var() {
        let mut env = SimEnv::new(
            HashMap::from([(String::from("NETSIM_IP_NODE"), String::from("10.0.0.2"))]),
            HashMap::from([(String::from("transfer"), String::from("/tmp/transfer-bin"))]),
        );
        env.set_capture("srv", "endpoint_id", String::from("abc123"));

        let s = env
            .interpolate_str("${binary.transfer} fetch ${srv.endpoint_id} $NETSIM_IP_NODE")
            .unwrap();
        assert_eq!(s, "/tmp/transfer-bin fetch abc123 10.0.0.2");
    }

    #[test]
    fn interpolate_unknown_dollar_var_is_left_as_is() {
        let env = SimEnv::new(HashMap::new(), HashMap::new());
        let s = env.interpolate_str("echo $UNSET_VAR").unwrap();
        assert_eq!(s, "echo $UNSET_VAR");
    }

    #[test]
    fn interpolate_unknown_brace_var_errors() {
        let env = SimEnv::new(HashMap::new(), HashMap::new());
        let err = env.interpolate_str("${binary.missing}").unwrap_err();
        assert!(err.to_string().contains("unknown binary"));
    }
}

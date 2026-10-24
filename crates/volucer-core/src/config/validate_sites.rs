//! The site table: host patterns, duplicates, and per-site settings.

use super::rules::{
    validate_front_controller, validate_host_pattern, validate_route, validate_statics,
};
use super::*;

impl Config {
    /// The site table: host patterns, duplicates, and per-site settings.    ///
    /// Separate from `validate` only because it is long; the two are one rule set
    /// and are always called together.
    pub(super) fn validate_sites(&self) -> Result<()> {
        // Host patterns, and the duplicate check. A host claimed by two sites is
        // the one mistake a reader cannot see from either site alone: one of them
        // would silently never be reached, and which one depends on map order.
        let mut claimed: BTreeMap<String, &str> = BTreeMap::new();
        for (name, site) in &self.sites {
            if site.hosts.is_empty() {
                return Err(Error::Config(format!(
                    "sites.{name}.hosts is empty, so this site would never be reached; list at \
                     least one host name"
                )));
            }
            if site.document_root.as_os_str().is_empty() {
                return Err(Error::Config(format!(
                    "sites.{name}.document_root is empty; set the directory holding this site"
                )));
            }

            for host in &site.hosts {
                validate_host_pattern(host, &format!("sites.{name}.hosts"))?;
                let key = host.trim().to_ascii_lowercase();
                if let Some(existing) = claimed.insert(key, name) {
                    return Err(Error::Config(format!(
                        "sites.{existing} and sites.{name} both claim the host {host:?}; the first \
                         one declared would be the only one ever reached"
                    )));
                }
            }

            if let Some(front) = &site.front_controller {
                validate_front_controller(front, &format!("sites.{name}.front_controller"))?;
            }

            if let Some(extensions) = &site.script_extensions {
                crate::script::ScriptExtensions::from_config(
                    extensions,
                    &format!("sites.{name}.script_extensions"),
                )?;
            }

            if let Some(routes) = &site.routes {
                for (index, route) in routes.iter().enumerate() {
                    validate_route(
                        route,
                        &format!("sites.{name}.routes"),
                        index,
                        &self.backends,
                    )?;
                }
            }

            if let Some(backend) = &site.default_backend {
                if !self.backends.contains_key(backend) {
                    return Err(Error::Config(format!(
                        "sites.{name}.default_backend {backend:?} does not match any configured \
                         backend"
                    )));
                }
            }

            if let Some(statics) = &site.static_files {
                let resolved = statics.applied_to(&self.server.statics());
                validate_statics(&resolved, &format!("sites.{name}.static_files"))?;
            }
            // `static_files = false` and `serve_only = "static"` are both a way of
            // saying "files only", so writing them together is a contradiction and
            // writing `false` beside `"backend"` is a double negative. Rejecting is
            // what keeps the resolved answer a single statement.
            if let Some(StaticsSetting::Switch(enabled)) = &site.static_files {
                match (enabled, site.serve_only) {
                    (true, Some(ServeOnly::Backend)) => {
                        return Err(Error::Config(format!(
                            "sites.{name}: static_files = true cannot be combined with \
                             serve_only = \"backend\", which skips the static layer"
                        )))
                    }
                    (false, Some(ServeOnly::Static)) => {
                        return Err(Error::Config(format!(
                            "sites.{name}: static_files = false cannot be combined with \
                             serve_only = \"static\"; write one of them"
                        )))
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }
}

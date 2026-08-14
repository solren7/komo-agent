//! The shared tool catalog: what the model is told exists, and what the
//! executor dispatches against.
//!
//! Two kinds of entry live here, and the difference is the whole design:
//!
//! * **Built-in tools** are registered during wiring and never leave. They are
//!   the process's identity.
//! * **Mounted tools** arrive later — an external plugin host connecting, an
//!   MCP server appearing — and can leave again. [`ToolCatalog::mount`] hands
//!   back a [`Registration`] whose drop takes them back out, so an owner that
//!   dies (a crashed plugin host, a cancelled task) cannot leave phantom tools
//!   the model will call and nothing will answer.
//!
//! ## Byte stability, and why the snapshot exists
//!
//! Tool schemas are serialized into every request, and a provider's prompt
//! cache matches on exact bytes: entries are name-sorted so the rendered block
//! depends on *which* tools exist, never on the order they were added.
//!
//! Changing the set at all invalidates the cached prefix from the first changed
//! schema token — once, after which it warms again. That is the honest price of
//! mounting a tool, and it is why mounts are batched ([`mount_all`]) rather than
//! applied one at a time: three tools arriving together should cost one
//! invalidation, not three.
//!
//! What must *not* happen is the set changing **inside** a turn. The model was
//! handed one set of schemas; if the executor later dispatches against a
//! different set, a call it was invited to make answers "unknown tool". So a
//! turn takes one [`CatalogSnapshot`] and uses it from its first round to its
//! last — mutations land in the catalog and are picked up by the *next* turn.
//!
//! [`mount_all`]: ToolCatalog::mount_all

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::domain::tool::Tool;

/// An immutable view of the catalog, pinned for the length of one turn.
///
/// Cheap to clone (one `Arc` inside the handle callers hold). The generation is
/// what it was taken at, so two snapshots can be compared for "did the set
/// change under us".
pub struct CatalogSnapshot {
    /// Name-keyed and therefore name-sorted on iteration — the byte-stability
    /// guarantee the provider's prompt cache depends on.
    tools: BTreeMap<String, Arc<dyn Tool>>,
    generation: u64,
}

impl CatalogSnapshot {
    /// The tool registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Every tool, name-sorted. This is what the model's schema block is built
    /// from and what a prompt's tool-name list must agree with.
    pub fn tools(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.values()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Which version of the catalog this is. Bumped by every mutation, so an
    /// unchanged generation means an unchanged rendered schema block — and a
    /// still-valid cached prefix.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// The mutable catalog every executor and model backend shares.
///
/// Reads are lock-free in the common case: the current [`CatalogSnapshot`] is
/// rebuilt on mutation and handed out by `Arc` clone, so a turn taking its
/// snapshot never waits on a writer and never rebuilds the map.
pub struct ToolCatalog {
    state: RwLock<State>,
}

struct State {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    generation: u64,
    /// The snapshot readers get, rebuilt whenever `tools` changes.
    current: Arc<CatalogSnapshot>,
}

impl Default for ToolCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(State {
                tools: BTreeMap::new(),
                generation: 0,
                current: Arc::new(CatalogSnapshot {
                    tools: BTreeMap::new(),
                    generation: 0,
                }),
            }),
        }
    }

    /// Add a tool for the life of the process — the wiring-time path.
    ///
    /// Deliberately returns nothing: a built-in tool has no owner that could
    /// outlive it, and handing back a guard that unregisters on drop would make
    /// `catalog.register(tool);` silently remove what it just added. Runtime
    /// mounts use [`mount`](Self::mount), whose guard is `#[must_use]`.
    pub fn register(&self, tool: Arc<dyn Tool>) {
        self.mutate(|tools| {
            tools.insert(tool.name().to_string(), tool);
        });
    }

    /// Mount a tool until the returned [`Registration`] is dropped.
    #[must_use = "dropping the registration unmounts the tool"]
    pub fn mount(self: &Arc<Self>, tool: Arc<dyn Tool>) -> Registration {
        self.mount_all(vec![tool])
    }

    /// Mount several tools as one change: one generation bump, so a plugin host
    /// arriving with a dozen tools costs the prompt cache one invalidation
    /// rather than a dozen. Dropping the guard unmounts all of them.
    ///
    /// A name already in the catalog is replaced, and dropping the guard removes
    /// it rather than restoring what was there — mounting over a built-in is a
    /// wiring mistake, not a feature to support.
    #[must_use = "dropping the registration unmounts the tools"]
    pub fn mount_all(self: &Arc<Self>, mounted: Vec<Arc<dyn Tool>>) -> Registration {
        let names: Vec<String> = mounted.iter().map(|t| t.name().to_string()).collect();
        self.mutate(|tools| {
            for tool in mounted {
                tools.insert(tool.name().to_string(), tool);
            }
        });
        Registration {
            catalog: Arc::downgrade(self),
            names,
        }
    }

    /// Remove every tool whose name `drop_it` accepts, returning what went
    /// (sorted). One mutation, so a policy sweep costs one generation bump.
    pub fn retain(&self, drop_it: impl Fn(&str) -> bool) -> Vec<String> {
        let mut removed = Vec::new();
        self.mutate(|tools| {
            removed = tools
                .keys()
                .filter(|name| drop_it(name))
                .cloned()
                .collect::<Vec<_>>();
            for name in &removed {
                tools.remove(name);
            }
        });
        removed
    }

    /// The current contents, pinned. A turn takes this once and uses it
    /// throughout, so a mount landing mid-turn cannot change what the turn
    /// dispatches against.
    pub fn snapshot(&self) -> Arc<CatalogSnapshot> {
        self.read().current.clone()
    }

    /// Apply a change and rebuild the snapshot readers will get.
    ///
    /// The generation is bumped unconditionally rather than by comparing maps:
    /// a spurious bump costs a cache invalidation that was going to happen
    /// anyway, while a missed one would leave turns dispatching against a stale
    /// snapshot forever.
    fn mutate(&self, change: impl FnOnce(&mut BTreeMap<String, Arc<dyn Tool>>)) {
        let mut state = match self.state.write() {
            Ok(state) => state,
            // A panicking writer left the map as it was mid-change; the
            // alternative to carrying on is a catalog that answers nothing for
            // the rest of the process.
            Err(poisoned) => poisoned.into_inner(),
        };
        change(&mut state.tools);
        state.generation += 1;
        state.current = Arc::new(CatalogSnapshot {
            tools: state.tools.clone(),
            generation: state.generation,
        });
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        match self.state.read() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Keeps mounted tools in the catalog. Dropping it takes them out.
///
/// Holds a *weak* reference: a registration outliving the catalog (a plugin
/// host shutting down after the runtime) is a no-op, not a resurrection.
pub struct Registration {
    catalog: std::sync::Weak<ToolCatalog>,
    names: Vec<String>,
}

impl Registration {
    /// The tools this registration keeps mounted.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Unmount now instead of at drop. Idempotent — the drop that follows finds
    /// nothing left to do.
    pub fn unmount(mut self) {
        self.remove();
    }

    fn remove(&mut self) {
        let names = std::mem::take(&mut self.names);
        if names.is_empty() {
            return;
        }
        let Some(catalog) = self.catalog.upgrade() else {
            return;
        };
        // One mutation for the whole registration: unmounting a plugin host's
        // dozen tools is one change to the model's view, like mounting them was.
        catalog.mutate(|tools| {
            // Reverse order, mirroring how effects unwind everywhere else — it
            // makes no difference to a map, and a reader who assumes it does
            // should be right.
            for name in names.iter().rev() {
                tools.remove(name);
            }
        });
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::context::ToolContext;
    use crate::domain::tool::{ToolError, ToolOutput};
    use async_trait::async_trait;

    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "stand-in"
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    fn tool(name: &'static str) -> Arc<dyn Tool> {
        Arc::new(NamedTool(name))
    }

    fn names(snapshot: &CatalogSnapshot) -> Vec<&str> {
        snapshot.names().collect()
    }

    /// Registration order must not reach the rendered schema block: the bytes a
    /// provider's cache matches on have to depend only on which tools exist.
    #[test]
    fn the_snapshot_is_name_sorted_whatever_the_registration_order() {
        let catalog = ToolCatalog::new();
        catalog.register(tool("zeta"));
        catalog.register(tool("alpha"));
        catalog.register(tool("mid"));
        assert_eq!(names(&catalog.snapshot()), vec!["alpha", "mid", "zeta"]);
    }

    /// The point of the whole module: a mount lasts exactly as long as its
    /// owner, so a dead plugin host leaves no tool the model can call and
    /// nothing can answer.
    #[test]
    fn a_mount_lasts_until_its_registration_drops() {
        let catalog = Arc::new(ToolCatalog::new());
        catalog.register(tool("builtin"));

        let mounted = catalog.mount(tool("plugin"));
        assert_eq!(names(&catalog.snapshot()), vec!["builtin", "plugin"]);

        drop(mounted);
        assert_eq!(names(&catalog.snapshot()), vec!["builtin"]);
    }

    /// A batch is one change, not N: a host arriving with several tools costs
    /// the prompt cache a single invalidation.
    #[test]
    fn a_batch_mounts_and_unmounts_as_one_generation() {
        let catalog = Arc::new(ToolCatalog::new());
        let before = catalog.snapshot().generation();

        let mounted = catalog.mount_all(vec![tool("a"), tool("b"), tool("c")]);
        let after = catalog.snapshot();
        assert_eq!(after.len(), 3);
        assert_eq!(after.generation(), before + 1, "one bump for the batch");

        drop(mounted);
        assert_eq!(catalog.snapshot().generation(), before + 2);
        assert!(catalog.snapshot().is_empty());
    }

    /// A turn pins its snapshot; a mount that lands mid-turn must not change
    /// what that turn dispatches against, or a call the model was invited to
    /// make would answer "unknown tool" a round later.
    #[test]
    fn a_pinned_snapshot_does_not_see_later_changes() {
        let catalog = Arc::new(ToolCatalog::new());
        catalog.register(tool("read"));
        let pinned = catalog.snapshot();

        let _mounted = catalog.mount(tool("late"));
        catalog.retain(|name| name == "read");

        assert_eq!(names(&pinned), vec!["read"], "the turn's view is frozen");
        assert!(pinned.get("read").is_some());
        assert_eq!(
            names(&catalog.snapshot()),
            vec!["late"],
            "the catalog moved"
        );
    }

    /// Unmounting early and then dropping the guard must not touch a tool that
    /// was re-mounted in between.
    #[test]
    fn an_explicit_unmount_is_not_repeated_at_drop() {
        let catalog = Arc::new(ToolCatalog::new());
        let first = catalog.mount(tool("plugin"));
        first.unmount();
        assert!(catalog.snapshot().is_empty());

        let _second = catalog.mount(tool("plugin"));
        assert_eq!(names(&catalog.snapshot()), vec!["plugin"]);
    }

    /// A registration that outlives its catalog is inert — a plugin host
    /// shutting down after the runtime must not panic or resurrect anything.
    #[test]
    fn a_registration_outliving_its_catalog_is_inert() {
        let catalog = Arc::new(ToolCatalog::new());
        let mounted = catalog.mount(tool("plugin"));
        drop(catalog);
        drop(mounted); // must not panic
    }

    #[test]
    fn retain_reports_what_it_removed_in_one_change() {
        let catalog = ToolCatalog::new();
        for name in ["read", "write", "shell"] {
            catalog.register(tool(name));
        }
        let before = catalog.snapshot().generation();

        let mut removed = catalog.retain(|name| name != "read");
        removed.sort();
        assert_eq!(removed, vec!["shell", "write"]);
        assert_eq!(names(&catalog.snapshot()), vec!["read"]);
        assert_eq!(catalog.snapshot().generation(), before + 1);
    }
}

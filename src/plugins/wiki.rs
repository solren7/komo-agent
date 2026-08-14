//! Note-vault search as a plugin (`[wiki]`). Degrades: a broken vector
//! backend costs `wiki_search`, never the boot.

use std::sync::Arc;

use async_trait::async_trait;

use komo_tools::wiki_index::WikiIndexTool;
use komo_tools::wiki_read::WikiReadTool;
use komo_tools::wiki_search::WikiSearchTool;

use super::{Plugin, Scope, ToolCx, ToolRegistry};
use crate::services::operator_control::actions::WikiOps;

pub struct WikiPlugin;

#[async_trait]
impl Plugin for WikiPlugin {
    fn name(&self) -> &'static str {
        "wiki"
    }

    async fn setup_tools(&self, reg: &mut ToolRegistry, cx: &ToolCx<'_>) -> anyhow::Result<()> {
        let Some(wiki) = &cx.config.runtime.wiki else {
            return Ok(());
        };
        // Registered before the handles are built, and kept even if they fail:
        // a broken vector backend costs search, not the ability to read a note
        // whose path the user or a memory already names.
        reg.tool(
            Scope::AGENTIC,
            Arc::new(WikiReadTool::new(wiki.vault.clone())),
        );

        let (index, embedder) = match wiki_handles(wiki) {
            Ok(handles) => handles,
            Err(error) => {
                tracing::warn!(error = format!("{error:#}"), "wiki_search unavailable");
                return Ok(());
            }
        };
        // Probed once so a wrong url still shows up at boot instead of on the
        // first search. The outcome is a diagnostic, never a decision.
        match index.get().await {
            Ok(_) => tracing::info!(vault = %wiki.vault.display(), "wiki_search ready"),
            Err(error) => tracing::warn!(
                error = format!("{error:#}"),
                "wiki index not open — wiki_search retries on each call"
            ),
        }
        // One runner shared by every indexing caller: this process's
        // `wiki_index` tool, `komo wiki index` over the operator channel, and
        // any cron job. Two concurrent runs over one store is not merely
        // wasteful — a rebuild resets it.
        let runner = Arc::new(komo_services::wiki_indexing::WikiIndexRunner::new(
            index.clone(),
            embedder.clone(),
            wiki.vault.clone(),
            wiki.embedding.model.clone(),
        ));
        reg.wiki_ops = Some(WikiOps {
            runner: runner.clone(),
            backend: wiki.backend.clone(),
            collection: wiki.collection.clone(),
            location: if wiki.backend == "server" {
                wiki.url.clone()
            } else {
                wiki.data_dir.join(&wiki.collection).display().to_string()
            },
        });
        reg.tool(
            Scope::AGENTIC,
            Arc::new(WikiSearchTool::new(index, embedder)),
        );
        reg.tool(Scope::AGENTIC, Arc::new(WikiIndexTool::new(runner)));
        Ok(())
    }
}

/// Build the note-vault handles: a lazily-opened index and its embedding
/// client. Neither touches the network here, so the only failures left are the
/// ones a running process can never recover from — a backend name that does
/// not parse, an embedding url that is not a url. Reaching the vault is
/// deferred to `LazyWikiIndex`, which retries it per call.
fn wiki_handles(
    wiki: &komo_config::WikiConfig,
) -> anyhow::Result<(
    Arc<komo_wiki::lazy::LazyWikiIndex>,
    Arc<dyn komo_core::domain::embedding::EmbeddingClient>,
)> {
    let index = komo_wiki::lazy::LazyWikiIndex::new(komo_wiki::WikiSettings {
        backend: komo_wiki::WikiBackend::parse(&wiki.backend)?,
        data_dir: wiki.data_dir.clone(),
        url: wiki.url.clone(),
        collection: wiki.collection.clone(),
        // Credentials come from the environment, never config.toml.
        api_key: std::env::var("QDRANT_API_KEY").ok(),
    });
    let embedder = komo_infra::embedding::OllamaEmbedder::new(
        wiki.embedding.url.clone(),
        wiki.embedding.model.clone(),
    )?;
    Ok((Arc::new(index), Arc::new(embedder)))
}

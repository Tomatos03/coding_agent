use super::embed::Embedder;
use super::store::{Chunk, InMemoryStore, SearchHit};

pub struct Retriever {
    embedder: Embedder,
    store: InMemoryStore,
}

impl Retriever {
    pub fn new(embedder: Embedder) -> Self {
        Self {
            embedder,
            store: InMemoryStore::default(),
        }
    }

    pub async fn ingest(&mut self, text: &str) -> anyhow::Result<()> {
        let vector = self.embedder.embed(text).await?;

        self.store.insert(Chunk {
            vector,
            text: text.to_owned(),
        })
    }

    pub async fn retrieve(&self, query: &str, top_k: usize) -> anyhow::Result<Vec<SearchHit>> {
        let vector = self.embedder.embed(query).await?;

        self.store.search(&vector, top_k)
    }

    pub fn len(&self) -> usize {
        self.store.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }
}

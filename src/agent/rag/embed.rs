use crate::agent::llm::provider;

use async_openai::config::OpenAIConfig;
use async_openai::types::embeddings::{
    CreateEmbeddingRequest, CreateEmbeddingRequestArgs, CreateEmbeddingResponse,
};

pub struct Embedder {
    model: String,
    client: async_openai::Client<OpenAIConfig>,
}

impl Embedder {
    pub fn new() -> anyhow::Result<Self> {
        let model = provider::embedding_model_id()?;
        let client = async_openai::Client::with_config(provider::embedding_client_config()?);

        Ok(Self { model, client })
    }

    pub async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let request = build_embedding_request(&self.model, text)?;
        let response = self.client.embeddings().create(request).await?;

        extract_embedding(response)
    }
}

fn build_embedding_request(model: &str, text: &str) -> anyhow::Result<CreateEmbeddingRequest> {
    Ok(CreateEmbeddingRequestArgs::default()
        .model(model)
        .input(text)
        .build()?)
}

fn extract_embedding(response: CreateEmbeddingResponse) -> anyhow::Result<Vec<f32>> {
    response
        .data
        .into_iter()
        .next()
        .map(|item| item.embedding)
        .ok_or_else(|| anyhow::anyhow!("embeddings 响应里没有任何向量"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::embeddings::{Embedding, EmbeddingUsage};

    fn response_with(vectors: Vec<Vec<f32>>) -> CreateEmbeddingResponse {
        CreateEmbeddingResponse {
            object: "list".to_owned(),
            model: "test-model".to_owned(),
            data: vectors
                .into_iter()
                .enumerate()
                .map(|(index, embedding)| Embedding {
                    index: index as u32,
                    object: "embedding".to_owned(),
                    embedding,
                })
                .collect(),
            usage: EmbeddingUsage {
                prompt_tokens: 1,
                total_tokens: 1,
            },
        }
    }

    #[test]
    fn request_body_carries_model_and_input() {
        let request = build_embedding_request("bge-m3", "你好，世界").expect("构造请求失败");
        let json = serde_json::to_value(&request).expect("序列化请求失败");

        assert_eq!(json["model"], "bge-m3");
        assert_eq!(json["input"], "你好，世界");
        assert!(
            json.get("encoding_format").is_none(),
            "不应显式发送 encoding_format（默认即 float），实际请求体：{json}"
        );
        assert!(json.get("dimensions").is_none());
    }

    #[test]
    fn extracts_the_first_vector() {
        let response = response_with(vec![vec![0.1, 0.2, 0.3], vec![0.4]]);

        assert_eq!(
            extract_embedding(response).expect("提取失败"),
            vec![0.1, 0.2, 0.3]
        );
    }

    #[test]
    fn errors_on_empty_data() {
        assert!(extract_embedding(response_with(vec![])).is_err());
    }

    #[tokio::test]
    #[ignore = "需要真实 EMBEDDING_* 凭证；手动运行 cargo test --lib -- --ignored"]
    async fn embeds_text_against_a_real_endpoint() {
        crate::bootstrap::init();
        let embedder = Embedder::new().expect("缺少 embedding 配置，请检查 .env");
        let vector = embedder
            .embed("你好，世界")
            .await
            .expect("embedding 请求失败");

        assert!(!vector.is_empty());
    }
}

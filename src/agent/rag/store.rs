#[derive(Debug)]
pub struct Chunk {
    pub vector: Vec<f32>,
    pub text: String,
}

#[derive(Debug)]
pub struct SearchHit {
    pub score: f32,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct InMemoryStore {
    chunks: Vec<Chunk>,
    dim: usize, // 向量维度
}

impl InMemoryStore {
    pub fn insert(&mut self, chunk: Chunk) -> anyhow::Result<()> {
        if chunk.vector.is_empty() {
            return Err(anyhow::anyhow!("向量为空，拒绝入库"));
        }

        let actual = chunk.vector.len();

        if self.chunks.is_empty() {
            self.dim = actual;
        } else if actual != self.dim {
            let expected = self.dim;
            return Err(anyhow::anyhow!(
                "向量维度不符：期望 {expected} 维，实际 {actual} 维"
            ));
        }

        self.chunks.push(chunk);

        Ok(())
    }

    pub fn search(&self, query: &[f32], top_k: usize) -> anyhow::Result<Vec<SearchHit>> {
        if self.chunks.is_empty() {
            return Ok(Vec::new());
        }

        let expected = self.dim;
        let actual = query.len();
        if actual != expected {
            return Err(anyhow::anyhow!(
                "查询向量维度不符：期望 {expected} 维，实际 {actual} 维"
            ));
        }

        let mut scored: Vec<(f32, &Chunk)> = self
            .chunks
            .iter()
            .map(|chunk| (cosine_similarity(query, &chunk.vector), chunk))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));

        Ok(scored
            .into_iter()
            .take(top_k)
            .map(|(score, chunk)| SearchHit {
                score,
                text: chunk.text.clone(),
            })
            .collect())
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

/// 向量相似度: 评估两个向量在空间中的距离远近.
/// 计算向量相似度的方法有多种, 这里使用余弦相似度
///
/// 余弦相似度计算公式（点积 ÷ 两模长之积）：
///
/// ```text
/// cos(a, b) = (a · b) / (‖a‖ · ‖b‖)
///
///   a · b = Σ aᵢbᵢ       点积：两组对应分量乘积之和
///   ‖a‖   = √(Σ aᵢ²)     模长：各分量平方和的平方根
/// ```
///
/// 取值范围 [-1, 1]：1 表示方向相同、0 表示正交、-1 表示方向相反。
/// 任一向量是零向量时没有方向可言，直接返回 0.0（避免除零产生 NaN）。
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "期望 {expected}，实际 {actual}"
        );
    }

    fn chunk(vector: Vec<f32>, text: &str) -> Chunk {
        Chunk {
            vector,
            text: text.to_owned(),
        }
    }

    fn sample_store() -> InMemoryStore {
        let mut store = InMemoryStore::default();
        store
            .insert(chunk(vec![1.0, 0.0], "正相关"))
            .expect("入库失败");
        store
            .insert(chunk(vec![1.0, 1.0], "斜相关"))
            .expect("入库失败");
        store
            .insert(chunk(vec![0.0, 1.0], "正交"))
            .expect("入库失败");
        store
            .insert(chunk(vec![-1.0, 0.0], "负相关"))
            .expect("入库失败");
        store
    }

    #[test]
    fn identical_vectors_score_one() {
        let vector = vec![1.0, 2.0, 3.0];

        assert_close(cosine_similarity(&vector, &vector), 1.0);
    }

    #[test]
    fn parallel_vectors_score_one_regardless_of_magnitude() {
        assert_close(cosine_similarity(&[1.0, 2.0], &[2.0, 4.0]), 1.0);
    }

    #[test]
    fn orthogonal_vectors_score_zero() {
        assert_close(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn opposite_vectors_score_minus_one() {
        assert_close(cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]), -1.0);
    }

    #[test]
    fn zero_vector_scores_zero() {
        assert_close(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn insert_rejects_empty_vector() {
        let mut store = InMemoryStore::default();

        assert!(store.insert(chunk(vec![], "空向量")).is_err());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn insert_rejects_dimension_mismatch() {
        let mut store = InMemoryStore::default();
        store
            .insert(chunk(vec![1.0, 0.0], "a"))
            .expect("首次入库失败");

        assert!(store.insert(chunk(vec![1.0, 0.0, 0.0], "b")).is_err());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn search_orders_hits_by_score_descending() {
        let hits = sample_store().search(&[1.0, 0.0], 10).expect("检索失败");

        let texts: Vec<&str> = hits.iter().map(|hit| hit.text.as_str()).collect();
        assert_eq!(texts, ["正相关", "斜相关", "正交", "负相关"]);
        assert_close(hits[0].score, 1.0);
        assert_close(hits[1].score, std::f32::consts::FRAC_1_SQRT_2);
        assert_close(hits[2].score, 0.0);
        assert_close(hits[3].score, -1.0);
    }

    #[test]
    fn search_respects_top_k() {
        let hits = sample_store().search(&[1.0, 0.0], 2).expect("检索失败");

        let texts: Vec<&str> = hits.iter().map(|hit| hit.text.as_str()).collect();
        assert_eq!(texts, ["正相关", "斜相关"]);
    }

    #[test]
    fn search_with_zero_top_k_returns_nothing() {
        let hits = sample_store().search(&[1.0, 0.0], 0).expect("检索失败");

        assert!(hits.is_empty());
    }

    #[test]
    fn search_on_empty_store_returns_nothing() {
        let hits = InMemoryStore::default()
            .search(&[1.0, 0.0], 3)
            .expect("检索失败");

        assert!(hits.is_empty());
    }

    #[test]
    fn search_rejects_query_dimension_mismatch() {
        assert!(sample_store().search(&[1.0, 0.0, 0.0], 3).is_err());
    }
}

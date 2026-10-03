//! 与业务无关的工具函数。
//!
//! 这里只放纯函数：哈希、文本归一化、锚点编解码、JSON Schema 后处理。依赖具体工具状态
//! （如锚点分配、服务记录）的逻辑不放在这里。

pub(crate) mod anchor;
pub(crate) mod hash;
pub(crate) mod schema;
pub(crate) mod text;

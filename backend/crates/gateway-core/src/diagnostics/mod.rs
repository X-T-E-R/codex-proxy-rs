//! 请求诊断与操作故障的观测合同，不参与路由、重试或客户端协议决策

mod capture;
mod failure;
mod selection;
mod stream;
mod trace;

pub use capture::{body_fingerprint, diagnostic_headers, diagnostic_json, json_fingerprint};
pub use failure::{OperationalDiagnostics, OperationalFailure};
pub use stream::{StreamCapture, StreamFormat};
pub use trace::TraceContext;

/// 完整紧凑 JSON 快照的 UTF-8 字节上限，包含事件、元数据和 JSON 转义
pub const MAX_DIAGNOSTIC_TRACE_BYTES: usize = 64 * 1024;

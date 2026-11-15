use std::ffi::CString;
use std::num::NonZeroU32;
use std::sync::Once;
use std::time::Instant;

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::mtmd::{
    MtmdBitmap, MtmdContext, MtmdContextParams, MtmdInputText, mtmd_default_marker,
};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::{LogOptions, send_logs_to_tracing};

use crate::convert::chandra_page;
use crate::engine::{OcrEngine, OcrOptions, PageOcr};
use crate::model::ModelPaths;
use crate::{OcrError, prompt};

/// Context size for the prompt (image tokens plus layout prompt), before the output budget.
const PROMPT_CTX: u32 = 8192;
const MAX_CTX: u32 = 16384;
const N_BATCH: u32 = 2048;
/// Qwen-VL needs at least 1024 image tokens (llama.cpp warns below that).
const IMAGE_MIN_TOKENS: i32 = 1024;
const MAX_DEFAULT_THREADS: usize = 8;

#[cfg(target_os = "macos")]
const GPU_LAYERS: u32 = 999;
#[cfg(not(target_os = "macos"))]
const GPU_LAYERS: u32 = 0;

/// Chandra OCR 2 running in-process through llama.cpp.
///
/// Only one engine can exist per process, because llama.cpp's backend is global.
pub struct LlamaEngine {
    // Field order is drop order: the mtmd context before the model, the model before the backend.
    mtmd: MtmdContext,
    model: LlamaModel,
    backend: LlamaBackend,
    opts: OcrOptions,
    threads: i32,
    label: String,
}

impl std::fmt::Debug for LlamaEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlamaEngine")
            .field("label", &self.label)
            .field("opts", &self.opts)
            .field("threads", &self.threads)
            .finish_non_exhaustive()
    }
}

fn engine_err(what: &str, e: impl std::fmt::Display) -> OcrError {
    OcrError::Engine(format!("{what}: {e}"))
}

/// Keeps llama.cpp, ggml and mtmd quiet unless `NOTEDOWN_LLAMA_LOG=1`.
fn configure_logging() {
    static LOGGING: Once = Once::new();
    LOGGING.call_once(|| {
        if std::env::var("NOTEDOWN_LLAMA_LOG").as_deref() != Ok("1") {
            send_logs_to_tracing(LogOptions::default().with_logs_enabled(false));
        }
    });
}

fn default_threads() -> i32 {
    let n = std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .min(MAX_DEFAULT_THREADS);
    i32::try_from(n).unwrap_or(4)
}

fn context_size(max_tokens: u32) -> u32 {
    PROMPT_CTX.saturating_add(max_tokens).min(MAX_CTX)
}

fn millis(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl LlamaEngine {
    /// Loads the language model and the vision projector.
    pub fn load(paths: &ModelPaths, opts: OcrOptions) -> Result<LlamaEngine, OcrError> {
        for path in [&paths.model, &paths.mmproj] {
            if !path.is_file() {
                return Err(OcrError::ModelMissing { path: path.clone() });
            }
        }
        configure_logging();
        let threads = opts.threads.unwrap_or_else(default_threads);
        let backend = LlamaBackend::init().map_err(|e| engine_err("llama.cpp backend", e))?;
        let model_params = LlamaModelParams::default().with_n_gpu_layers(GPU_LAYERS);
        let model = LlamaModel::load_from_file(&backend, &paths.model, &model_params)
            .map_err(|e| engine_err(&format!("loading {}", paths.model.display()), e))?;
        let mmproj = paths.mmproj.to_str().ok_or_else(|| {
            OcrError::Engine(format!("path is not UTF-8: {}", paths.mmproj.display()))
        })?;
        let mtmd_params = MtmdContextParams {
            use_gpu: cfg!(target_os = "macos"),
            print_timings: false,
            n_threads: threads,
            media_marker: CString::new(mtmd_default_marker())
                .map_err(|e| engine_err("media marker", e))?,
            image_min_tokens: IMAGE_MIN_TOKENS,
            image_max_tokens: -1,
        };
        let mtmd = MtmdContext::init_from_file(mmproj, &model, &mtmd_params)
            .map_err(|e| engine_err(&format!("loading {}", paths.mmproj.display()), e))?;
        if !mtmd.support_vision() {
            return Err(OcrError::Engine(format!(
                "{} has no vision support",
                paths.mmproj.display()
            )));
        }
        Ok(LlamaEngine {
            mtmd,
            model,
            backend,
            opts,
            threads,
            label: paths.label.clone(),
        })
    }
}

impl OcrEngine for LlamaEngine {
    fn ocr_png(&mut self, png: &[u8]) -> Result<PageOcr, OcrError> {
        let n_ctx = context_size(self.opts.max_tokens);
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(n_ctx))
            .with_n_batch(N_BATCH)
            .with_n_threads(self.threads)
            .with_n_threads_batch(self.threads);
        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
            .map_err(|e| engine_err("creating llama context", e))?;

        let encode_start = Instant::now();
        let bitmap = MtmdBitmap::from_buffer(&self.mtmd, png, false)
            .map_err(|e| engine_err("decoding page image", e))?;
        let chunks = self
            .mtmd
            .tokenize(
                MtmdInputText {
                    text: prompt::chat_prompt(mtmd_default_marker()),
                    add_special: true,
                    parse_special: true,
                },
                &[&bitmap],
            )
            .map_err(|e| engine_err("tokenizing prompt", e))?;
        let prompt_tokens = u32::try_from(chunks.total_tokens()).unwrap_or(u32::MAX);
        if prompt_tokens >= n_ctx {
            return Err(OcrError::Engine(format!(
                "prompt needs {prompt_tokens} tokens, context holds {n_ctx}"
            )));
        }
        #[allow(clippy::cast_possible_wrap)] // N_BATCH is a small constant
        let mut n_past = chunks
            .eval_chunks(&self.mtmd, &ctx, 0, 0, N_BATCH as i32, true)
            .map_err(|e| engine_err("evaluating prompt", e))?;
        let encode_ms = millis(encode_start);

        let decode_start = Instant::now();
        let vocab = self.model.vocab();
        let mut sampler = LlamaSampler::greedy();
        let mut batch = LlamaBatch::new(1, 1);
        let mut bytes = Vec::new();
        let mut output_tokens = 0u32;
        let mut truncated = true;
        let budget = self
            .opts
            .max_tokens
            .min(n_ctx.saturating_sub(prompt_tokens));
        while output_tokens < budget {
            // `sample` also accepts the token into the sampler.
            let token = sampler.sample(&ctx, -1);
            if vocab.is_eog(token) {
                truncated = false;
                break;
            }
            vocab.token_to_piece_into(token, &mut bytes, true, None);
            output_tokens += 1;
            if output_tokens == budget {
                break;
            }
            batch.clear();
            batch
                .add(token, n_past, &[0], true)
                .map_err(|e| engine_err("building batch", e))?;
            ctx.decode(&mut batch)
                .map_err(|e| engine_err("decoding", e))?;
            n_past += 1;
        }
        let decode_ms = millis(decode_start);

        let text = String::from_utf8_lossy(&bytes);
        Ok(PageOcr {
            prompt_tokens,
            output_tokens,
            truncated,
            encode_ms,
            decode_ms,
            ..chandra_page(prompt::strip_think(&text))
        })
    }

    fn label(&self) -> &str {
        &self.label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_size_adds_output_budget_and_caps() {
        assert_eq!(context_size(4096), 12288);
        assert_eq!(context_size(100_000), MAX_CTX);
        assert_eq!(context_size(u32::MAX), MAX_CTX);
    }

    #[test]
    fn default_threads_is_bounded() {
        let t = default_threads();
        assert!((1..=8).contains(&t), "{t}");
    }

    #[test]
    fn load_reports_missing_model() {
        let paths = ModelPaths {
            model: "/nonexistent/model.gguf".into(),
            mmproj: "/nonexistent/mmproj.gguf".into(),
            label: "x".into(),
        };
        let err = LlamaEngine::load(&paths, OcrOptions::default()).unwrap_err();
        assert!(matches!(err, OcrError::ModelMissing { .. }), "{err}");
    }
}

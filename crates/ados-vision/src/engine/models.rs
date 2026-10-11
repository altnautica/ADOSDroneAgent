//! Model registry, inference and capability resolution.

use super::*;

impl VisionEngine {
    /// Register one of the engine's own configured models. Engine-run models are
    /// loaded on the backend now (a load failure falls back to recording the
    /// model without a loaded handle, so a missing model file or sidecar never
    /// rejects the registration). Returns the resolved execution and whether a
    /// backend model was loaded.
    pub async fn register_model(&self, meta: ModelMetadata) -> Result<(ModelExecution, bool)> {
        let (_, execution, had_backend) = self.insert_model(None, meta).await?;
        Ok((execution, had_backend))
    }

    /// Register a plugin's model under the plugin's namespace
    /// ([`owned_model_id`]). Refused when the namespaced id is held by someone
    /// else, or when the plugin already holds [`MAX_MODELS_PER_OWNER`] other
    /// models. Returns the registry id, the execution and whether a backend
    /// model was loaded.
    pub async fn register_owned_model(
        &self,
        owner: &str,
        mut meta: ModelMetadata,
    ) -> Result<(String, ModelExecution, bool)> {
        if owner.is_empty() || meta.id.is_empty() {
            return Err(anyhow!("a plugin model needs an owner and an id"));
        }
        meta.id = owned_model_id(owner, &meta.id);
        self.insert_model(Some(owner.to_string()), meta).await
    }

    pub(super) async fn insert_model(
        &self,
        owner: Option<String>,
        meta: ModelMetadata,
    ) -> Result<(String, ModelExecution, bool)> {
        let execution = meta.execution;
        let loaded: Option<Arc<dyn LoadedModel>> = if execution == ModelExecution::EngineRun {
            match run_blocking(|| self.backend.load(&meta)) {
                Ok(m) => Some(Arc::from(m)),
                Err(e) => {
                    tracing::warn!(model = %meta.id, error = %e, "model_load_failed; recorded without backend");
                    None
                }
            }
        } else {
            None
        };
        let had_backend = loaded.is_some();
        let id = meta.id.clone();
        let mut models = self.models.lock().await;
        if let Some(owner) = owner.as_deref() {
            match models.get(&id) {
                Some(existing) if existing.owner.as_deref() != Some(owner) => {
                    return Err(anyhow!("model id {id} is registered by someone else"));
                }
                Some(_) => {}
                None => {
                    let held = models
                        .values()
                        .filter(|m| m.owner.as_deref() == Some(owner))
                        .count();
                    if held >= MAX_MODELS_PER_OWNER {
                        return Err(anyhow!(
                            "plugin {owner} already holds {MAX_MODELS_PER_OWNER} registered models"
                        ));
                    }
                }
            }
        }
        models.insert(
            id.clone(),
            RegisteredModel {
                meta,
                owner,
                loaded,
            },
        );
        Ok((id, execution, had_backend))
    }

    /// Drop every model `owner` registered, and their timings. Called when the
    /// plugin host stops serving the plugin. Returns how many were dropped.
    pub async fn unregister_owner(&self, owner: &str) -> usize {
        let removed: Vec<String> = {
            let mut models = self.models.lock().await;
            let ids: Vec<String> = models
                .iter()
                .filter(|(_, m)| m.owner.as_deref() == Some(owner))
                .map(|(id, _)| id.clone())
                .collect();
            for id in &ids {
                models.remove(id);
            }
            ids
        };
        let mut timings = self.timings.lock().await;
        for id in &removed {
            timings.remove(id);
        }
        removed.len()
    }

    /// Number of registered models.
    pub async fn model_count(&self) -> usize {
        self.models.lock().await.len()
    }

    /// Registered model ids whose task (kind) matches `kind`, sorted for a
    /// deterministic result. Lets a consumer request perception by TASK ("a
    /// detection model", "a depth model") instead of naming one global
    /// detector, so several tasks can be selected and paced together.
    pub async fn models_for_kind(&self, kind: ados_protocol::framebus::ModelKind) -> Vec<String> {
        let models = self.models.lock().await;
        let mut ids: Vec<String> = models
            .values()
            .filter(|m| m.meta.kind == kind)
            .map(|m| m.meta.id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// A read-back of every registered model (id, task, execution, whether a
    /// backend loaded, output classes), sorted by id. The GCS vision hub shows
    /// this so an operator sees every model loaded on the drone, not only the
    /// ones actively publishing detections.
    pub async fn list_models(&self) -> Vec<ados_protocol::framebus::ModelInfo> {
        let models = self.models.lock().await;
        // Lock order models → timings (record_timing only ever holds timings).
        let timings = self.timings.lock().await;
        let backend_capable = self.backend.is_inference_capable();
        let mut out: Vec<ados_protocol::framebus::ModelInfo> = models
            .values()
            .map(|m| {
                let t = timings.get(&m.meta.id);
                ados_protocol::framebus::ModelInfo {
                    id: m.meta.id.clone(),
                    kind: m.meta.kind,
                    execution: m.meta.execution,
                    backend_loaded: m.loaded.is_some(),
                    output_classes: m.meta.output_classes.clone(),
                    fps: t.map(|t| t.fps()).unwrap_or(0.0),
                    latency_ms: t.map(|t| t.ewma_latency_ms).unwrap_or(0.0),
                    // A model runs a real detector only when its file loaded AND the
                    // engine's backend is inference-capable (a mock backend is not),
                    // so a placeholder is never presented as a working detector.
                    is_inference_capable: m.loaded.is_some() && backend_capable,
                }
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Resolve a perception capability: the first inference-capable registered
    /// model of `kind` whose `output_classes` covers `class` (or any model of
    /// that kind when `class` is `None`), by id order. `None` when nothing
    /// matches — the honest "this node cannot do that" answer instead of
    /// hand-picking a global detector.
    ///
    /// Only inference-capable models resolve: a mock-backed or failed-to-load
    /// model is never returned as a working capability. The result is
    /// deterministic because [`Self::list_models`] is sorted by id.
    pub async fn resolve_capability(
        &self,
        kind: ados_protocol::framebus::ModelKind,
        class: Option<&str>,
    ) -> Option<ados_protocol::framebus::ModelInfo> {
        self.list_models()
            .await
            .into_iter()
            .filter(|m| m.kind == kind && m.is_inference_capable)
            .find(|m| match class {
                Some(c) => m.output_classes.iter().any(|oc| oc == c),
                None => true,
            })
    }

    /// Run inference for `model_id` on a frame, serialized on the accelerator
    /// lease. Returns the detections. Errors when the model is unknown, is
    /// plugin-side (the plugin must run it), or has no loaded backend model.
    pub async fn infer(
        &self,
        model_id: &str,
        frame: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Result<Vec<Detection>> {
        self.run_model(None, model_id, frame, width, height, format)
            .await
    }

    /// [`Self::infer`] on behalf of plugin `owner`. `model_id` names the
    /// plugin's own model ([`owned_model_id`]) when it has one by that id, else
    /// one of the engine's configured models. Another plugin's model is refused.
    pub async fn infer_for(
        &self,
        owner: &str,
        model_id: &str,
        frame: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Result<Vec<Detection>> {
        self.run_model(Some(owner), model_id, frame, width, height, format)
            .await
    }

    pub(super) async fn run_model(
        &self,
        caller: Option<&str>,
        model_id: &str,
        frame: &[u8],
        width: u32,
        height: u32,
        format: FrameFormat,
    ) -> Result<Vec<Detection>> {
        // Acquire the accelerator lease for the duration of the inference so two
        // models never contend for the shared NPU. The permit is held across the
        // blocking call below.
        let _permit = self
            .accel_lease
            .acquire()
            .await
            .map_err(|_| anyhow!("accelerator lease closed"))?;

        // Take a handle on the loaded model and release the registry lock before
        // the (possibly slow) backend call, so model listing and registration
        // never wait behind an inference.
        let (key, loaded) = {
            let models = self.models.lock().await;
            let key = match caller {
                Some(owner) => {
                    let owned = owned_model_id(owner, model_id);
                    if models.contains_key(&owned) {
                        owned
                    } else {
                        model_id.to_string()
                    }
                }
                None => model_id.to_string(),
            };
            let reg = models
                .get(&key)
                .ok_or_else(|| anyhow!("unknown model {model_id}"))?;
            if let (Some(owner), Some(holder)) = (caller, reg.owner.as_deref()) {
                if holder != owner {
                    return Err(anyhow!("model {model_id} belongs to another plugin"));
                }
            }
            if reg.meta.execution == ModelExecution::PluginSide {
                return Err(anyhow!(
                    "model {model_id} is plugin-side; the plugin runs it"
                ));
            }
            let loaded = reg
                .loaded
                .clone()
                .ok_or_else(|| anyhow!("model {model_id} has no loaded backend"))?;
            (key, loaded)
        };
        let t0 = std::time::Instant::now();
        let result = run_blocking(|| loaded.infer(frame, width, height, format));
        let latency_ms = t0.elapsed().as_secs_f32() * 1000.0;
        if result.is_ok() {
            self.record_timing(&key, latency_ms, now_ms()).await;
        }
        result
    }
}

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};
use zkcl::{PersistentRoot, Root, Runtime};

pub(super) type ObjectError = Box<dyn std::error::Error + Send + Sync>;

/// IDs belong to the connection and are never reused, even after replacing the runtime.
#[derive(Clone, Default)]
pub(super) struct Objects(Arc<Mutex<ObjectTable>>);

#[derive(Default)]
struct ObjectTable {
    last_id: u64,
    roots: HashMap<u64, PersistentRoot>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
pub(super) enum ObjectRequest {
    Inspect {
        handle: u64,
        #[serde(default)]
        start: usize,
        #[serde(default = "page_size")]
        count: usize,
    },
    Modify {
        handle: u64,
        index: usize,
        value: ValueInput,
    },
    Release {
        handles: Vec<u64>,
    },
}

fn page_size() -> usize {
    32
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(super) enum ValueInput {
    Handle { handle: u64 },
    Source { source: String },
}

impl ValueInput {
    pub(super) fn resolve<'rt>(
        &self,
        objects: &Objects,
        runtime: &'rt Runtime,
    ) -> Result<Root<'rt>, ObjectError> {
        match self {
            Self::Handle { handle } => objects.get(runtime, *handle),
            Self::Source { source } => Ok(runtime
                .eval(source)?
                .into_iter()
                .next()
                .unwrap_or_else(|| runtime.nil())),
        }
    }
}

impl Objects {
    pub(super) fn retain(
        &self,
        runtime: &Runtime,
        object: &Root<'_>,
    ) -> Result<Value, ObjectError> {
        let mut descriptors = self.retain_all(runtime, [object])?;
        Ok(descriptors.remove(0))
    }

    pub(super) fn retain_all<'a, 'rt: 'a>(
        &self,
        runtime: &Runtime,
        values: impl IntoIterator<Item = &'a Root<'rt>>,
    ) -> Result<Vec<Value>, ObjectError> {
        // Prepare every descriptor before exposing any handles from this batch.
        let prepared = values
            .into_iter()
            .map(|value| {
                Ok((
                    value,
                    runtime.preview(value)?,
                    runtime.inspection_type(value)?,
                ))
            })
            .collect::<Result<Vec<_>, ObjectError>>()?;
        let mut table = self.0.lock().unwrap();
        table
            .last_id
            .checked_add(prepared.len().try_into()?)
            .ok_or("object handle space exhausted")?;
        Ok(prepared
            .into_iter()
            .map(|(value, preview, kind)| {
                table.last_id += 1;
                let id = table.last_id;
                table.roots.insert(id, value.clone().into_persistent());
                json!({"handle":id,"preview":preview,"type":kind})
            })
            .collect())
    }

    pub(super) fn get<'rt>(
        &self,
        runtime: &'rt Runtime,
        id: u64,
    ) -> Result<Root<'rt>, ObjectError> {
        self.0
            .lock()
            .unwrap()
            .roots
            .get(&id)
            .and_then(|root| root.bind(runtime))
            .ok_or_else(|| format!("unknown or released object handle {id}").into())
    }

    pub(super) fn clear(&self) {
        self.0.lock().unwrap().roots.clear();
    }

    pub(super) fn release(&self, handles: Vec<u64>) -> Vec<u64> {
        let mut table = self.0.lock().unwrap();
        handles
            .into_iter()
            .filter(|handle| table.roots.remove(handle).is_some())
            .collect()
    }

    pub(super) fn execute(
        &self,
        runtime: &Runtime,
        request: ObjectRequest,
    ) -> Result<Value, ObjectError> {
        match request {
            ObjectRequest::Inspect {
                handle,
                start,
                count,
            } => {
                if count > 1024 {
                    return Err("object inspection page cannot exceed 1024 components".into());
                }
                let object = self.get(runtime, handle)?;
                let page = runtime.inspect_object(&object, start, count)?;
                let mut values = self
                    .retain_all(
                        runtime,
                        page.parts.iter().filter_map(|part| part.value.as_ref()),
                    )?
                    .into_iter();
                let parts = page.parts.into_iter().map(|part| json!({
                    "index":part.index,"label":part.label,"writable":part.writable,
                    "value":part.value.map(|_| values.next().expect("retained inspector value"))
                })).collect::<Vec<_>>();
                Ok(
                    json!({"handle":handle,"summary":page.summary,"start":start,"total":page.total,"parts":parts}),
                )
            }
            ObjectRequest::Modify {
                handle,
                index,
                value,
            } => {
                let object = self.get(runtime, handle)?;
                let value = value.resolve(self, runtime)?;
                runtime.replace_inspected_part(&object, index, &value)?;
                self.retain(runtime, &value)
            }
            ObjectRequest::Release { handles } => Ok(json!({"released":self.release(handles)})),
        }
    }
}

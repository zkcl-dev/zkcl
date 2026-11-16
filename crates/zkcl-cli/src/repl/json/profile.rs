use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use zkcl::{EvalError, ProfileReport, ProfilerConfig, Runtime};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
pub(super) enum ProfileRequest {
    Start {
        #[serde(default = "interval", rename = "sampleIntervalMicros")]
        sample_interval_micros: u64,
    },
    Stop,
    Report,
    Reset,
}
fn interval() -> u64 {
    1000
}
fn encode(report: ProfileReport) -> Value {
    json!({"running":report.running,"elapsedNanos":report.elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,"samplingMode":report.sampling_mode,"samples":report.samples,"allocatedBytes":report.allocated_bytes,"collections":report.collections,"gcTimeNanos":report.gc_time.as_nanos().min(u128::from(u64::MAX)) as u64,
    "functions":report.functions.iter().map(|row|json!({"id":row.id,"name":row.name,"calls":row.calls,"nativeCalls":row.native_calls,"totalTimeNanos":row.total_time.as_nanos().min(u128::from(u64::MAX)) as u64,"selfTimeNanos":row.self_time.as_nanos().min(u128::from(u64::MAX)) as u64,"samples":row.samples,"allocatedBytes":row.allocated_bytes,"selfAllocatedBytes":row.self_allocated_bytes})).collect::<Vec<_>>()})
}
impl ProfileRequest {
    pub(super) fn execute(self, runtime: &Runtime) -> Result<Value, EvalError> {
        match self {
            Self::Start {
                sample_interval_micros,
            } => {
                runtime.start_profiling(ProfilerConfig {
                    sample_interval: Duration::from_micros(sample_interval_micros),
                })?;
                Ok(encode(runtime.profile_report()))
            }
            Self::Stop => Ok(encode(runtime.stop_profiling())),
            Self::Report => Ok(encode(runtime.profile_report())),
            Self::Reset => {
                runtime.reset_profiling();
                Ok(Value::Null)
            }
        }
    }
}

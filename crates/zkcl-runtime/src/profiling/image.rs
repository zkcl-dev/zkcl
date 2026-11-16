use super::*;
use crate::image::{ImageReader, ImageWriter};

impl Profiler {
    pub(crate) fn check_image_idle(&self) -> Result<(), EvalError> {
        if self.enabled.get() {
            return Err(EvalError::new(
                0,
                "stop the profiler before saving a runtime image",
            ));
        }
        Ok(())
    }

    pub(crate) fn write_image(&self, image: &mut ImageWriter<'_>) -> Result<(), EvalError> {
        self.check_image_idle()?;
        let session = self.session.borrow();
        let Some(session) = session.as_ref() else {
            image.bool(false);
            return Ok(());
        };
        let session = session.borrow();
        let report = session
            .stopped
            .as_ref()
            .expect("disabled profiler has stopped report");
        image.bool(true);
        write_duration(image, session.interval);
        write_duration(image, report.elapsed);
        image.u64(report.samples);
        image.u64(report.allocated_bytes);
        image.u64(report.collections);
        write_duration(image, report.gc_time);
        image.usize(report.functions.len());
        for row in &report.functions {
            image.u64(row.id);
            image.string(&row.name);
            image.u64(row.calls);
            image.u64(row.native_calls);
            write_duration(image, row.total_time);
            write_duration(image, row.self_time);
            image.u64(row.samples);
            image.u64(row.allocated_bytes);
            image.u64(row.self_allocated_bytes);
        }
        Ok(())
    }

    pub(crate) fn read_image(image: &mut ImageReader<'_>) -> Result<Self, EvalError> {
        let profiler = Self::default();
        if !image.bool()? {
            return Ok(profiler);
        }
        let interval = read_duration(image)?;
        if interval.is_zero() {
            return Err(invalid());
        }
        let elapsed = read_duration(image)?;
        let samples = image.u64()?;
        let allocated_bytes = image.u64()?;
        let collections = image.u64()?;
        let gc_time = read_duration(image)?;
        let mut functions = Vec::new();
        for _ in 0..image.usize()? {
            let row = FunctionProfile {
                id: image.u64()?,
                name: image.string()?,
                calls: image.u64()?,
                native_calls: image.u64()?,
                total_time: read_duration(image)?,
                self_time: read_duration(image)?,
                samples: image.u64()?,
                allocated_bytes: image.u64()?,
                self_allocated_bytes: image.u64()?,
            };
            if row.native_calls > row.calls
                || row.self_time > row.total_time
                || row.self_allocated_bytes > row.allocated_bytes
            {
                return Err(invalid());
            }
            functions.push(row);
        }
        let report = ProfileReport {
            running: false,
            elapsed,
            sampling_mode: SAMPLING_MODE,
            samples,
            allocated_bytes,
            collections,
            gc_time,
            functions,
        };
        // Report IDs are historical and do not retain or refer to live code.
        // A subsequent START-PROFILING creates a fresh session, so restored IDs
        // can never be merged with the destination runtime's definition IDs.
        *profiler.session.borrow_mut() = Some(Rc::new(RefCell::new(Session {
            timer: Timer::start(),
            running: false,
            interval,
            last_event: Duration::ZERO,
            last_sample: Duration::ZERO,
            last_allocated: 0,
            allocation_counter: Rc::default(),
            initial_allocated: 0,
            initial_collections: 0,
            initial_gc_time: Duration::ZERO,
            next_token: 0,
            samples: 0,
            functions: BTreeMap::new(),
            active: Vec::new(),
            current_task: None,
            tasks: BTreeMap::new(),
            stopped: Some(report),
        })));
        Ok(profiler)
    }
}

fn invalid() -> EvalError {
    EvalError::new(0, "invalid profiler state in runtime image")
}
fn write_duration(image: &mut ImageWriter<'_>, duration: Duration) {
    image.u64(duration.as_secs());
    image.u64(u64::from(duration.subsec_nanos()));
}
fn read_duration(image: &mut ImageReader<'_>) -> Result<Duration, EvalError> {
    let seconds = image.u64()?;
    let nanos = image.u64()?;
    if nanos >= 1_000_000_000 {
        return Err(invalid());
    }
    Ok(Duration::new(seconds, nanos as u32))
}

use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

#[pyfunction]
#[pyo3(signature = (path, strict=None))]
fn open(path: String, strict: Option<bool>) -> PyResult<Py<PyFile>> {
    let reader = blf_core::File::open_with_options(path, strict.unwrap_or(false))
        .map_err(|error| PyException::new_err(error.to_string()))?;
    Python::with_gil(|py| Py::new(py, PyFile { reader }))
}

#[pyclass(name = "File")]
struct PyFile {
    reader: blf_core::File,
}

#[pymethods]
impl PyFile {
    #[getter]
    fn file_statistics(&self) -> PyResult<PyObject> {
        Python::with_gil(|py| statistics_to_dict(py, &self.reader.file_statistics))
    }

    fn is_open(&self) -> bool {
        self.reader.is_open()
    }

    fn eof(&self) -> bool {
        self.reader.eof()
    }

    fn close(&mut self) {
        self.reader.close();
    }

    fn read(&mut self, py: Python<'_>) -> PyResult<Option<PyObject>> {
        match self
            .reader
            .read()
            .map_err(|error| PyException::new_err(error.to_string()))?
        {
            Some(object) => object_to_dict(py, &object).map(Some),
            None => Ok(None),
        }
    }

    fn read_batch(&mut self, py: Python<'_>, max_objects: usize) -> PyResult<Vec<PyObject>> {
        let objects = self
            .reader
            .read_batch(max_objects)
            .map_err(|error| PyException::new_err(error.to_string()))?;
        objects
            .iter()
            .map(|object| object_to_dict(py, object))
            .collect()
    }
}

fn statistics_to_dict(py: Python<'_>, stats: &blf_core::FileStatistics) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("signature", stats.signature)?;
    dict.set_item("statisticsSize", stats.statistics_size)?;
    dict.set_item("apiNumber", stats.api_number)?;
    dict.set_item("applicationId", stats.application_id)?;
    dict.set_item("compressionLevel", stats.compression_level)?;
    dict.set_item("applicationMajor", stats.application_major)?;
    dict.set_item("applicationMinor", stats.application_minor)?;
    dict.set_item("fileSize", stats.file_size)?;
    dict.set_item("uncompressedFileSize", stats.uncompressed_file_size)?;
    dict.set_item("objectCount", stats.object_count)?;
    dict.set_item("applicationBuild", stats.application_build)?;
    dict.set_item(
        "measurementStartTime",
        system_time_to_dict(py, &stats.measurement_start_time)?,
    )?;
    dict.set_item(
        "lastObjectTime",
        system_time_to_dict(py, &stats.last_object_time)?,
    )?;
    dict.set_item("restorePointsOffset", stats.restore_points_offset)?;
    dict.set_item(
        "reservedFileStatistics",
        stats.reserved_file_statistics.to_vec(),
    )?;
    Ok(dict.into_pyobject(py)?.into_any().unbind())
}

fn system_time_to_dict(py: Python<'_>, time: &blf_core::SystemTime) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("year", time.year)?;
    dict.set_item("month", time.month)?;
    dict.set_item("dayOfWeek", time.day_of_week)?;
    dict.set_item("day", time.day)?;
    dict.set_item("hour", time.hour)?;
    dict.set_item("minute", time.minute)?;
    dict.set_item("second", time.second)?;
    dict.set_item("milliseconds", time.milliseconds)?;
    Ok(dict.into_pyobject(py)?.into_any().unbind())
}

fn object_to_dict(py: Python<'_>, object: &blf_core::Object) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("objectType", object.object_type().as_u32())?;

    match object {
        blf_core::Object::CanMessage(value) => {
            dict.set_item("class", "CanMessage")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("flags", value.flags)?;
            dict.set_item("dlc", value.dlc)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", value.data.to_vec())?;
            dict.set_item("objectTimeStamp", value.header.object_time_stamp)?;
        }
        blf_core::Object::CanMessage2(value) => {
            dict.set_item("class", "CanMessage2")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("flags", value.flags)?;
            dict.set_item("dlc", value.dlc)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", value.data.clone())?;
            dict.set_item("frameLength", value.frame_length)?;
            dict.set_item("bitCount", value.bit_count)?;
            dict.set_item("objectTimeStamp", value.header.object_time_stamp)?;
        }
        blf_core::Object::CanFdMessage(value) => {
            dict.set_item("class", "CanFdMessage")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("flags", value.flags)?;
            dict.set_item("dlc", value.dlc)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", value.data.to_vec())?;
            dict.set_item("frameLength", value.frame_length)?;
            dict.set_item("objectTimeStamp", value.header.object_time_stamp)?;
        }
        blf_core::Object::CanFdMessage64(value) => {
            dict.set_item("class", "CanFdMessage64")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("dlc", value.dlc)?;
            dict.set_item("validDataBytes", value.valid_data_bytes)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", value.data.clone())?;
            dict.set_item("frameLength", value.frame_length)?;
            dict.set_item("objectTimeStamp", value.header.object_time_stamp)?;
        }
        blf_core::Object::CanErrorFrame(value) => {
            dict.set_item("class", "CanErrorFrame")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("length", value.length)?;
            dict.set_item("objectTimeStamp", value.header.object_time_stamp)?;
        }
        blf_core::Object::CanErrorFrameExt(value) => {
            dict.set_item("class", "CanErrorFrameExt")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", value.data.clone())?;
            dict.set_item("objectTimeStamp", value.header.object_time_stamp)?;
        }
        blf_core::Object::CanFdErrorFrame64(value) => {
            dict.set_item("class", "CanFdErrorFrame64")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", value.data.clone())?;
            dict.set_item("objectTimeStamp", value.header.object_time_stamp)?;
        }
        blf_core::Object::AppText(value) => {
            dict.set_item("class", "AppText")?;
            dict.set_item("source", value.source)?;
            dict.set_item("text", value.text.clone())?;
            dict.set_item("objectTimeStamp", value.header.object_time_stamp)?;
        }
        blf_core::Object::RestorePointContainer(value) => {
            dict.set_item("class", "RestorePointContainer")?;
            dict.set_item("dataLength", value.data_length)?;
            dict.set_item("data", value.data.clone())?;
            dict.set_item("objectTimeStamp", value.header.object_time_stamp)?;
        }
        blf_core::Object::Lin(value) => {
            dict.set_item("class", lin_class_name(value.object_type))?;
            dict.set_item("objectType", value.object_type.as_u32())?;
            dict.set_item("objectTimeStamp", value.header.object_time_stamp)?;
            for (name, field) in &value.fields {
                dict.set_item(name, field_value_to_py(py, field)?)?;
            }
        }
    }

    Ok(dict.into_pyobject(py)?.into_any().unbind())
}

fn lin_class_name(object_type: blf_core::ObjectType) -> &'static str {
    match object_type {
        blf_core::ObjectType::LinMessage => "LinMessage",
        blf_core::ObjectType::LinCrcError => "LinCrcError",
        blf_core::ObjectType::LinDlcInfo => "LinDlcInfo",
        blf_core::ObjectType::LinRcvError => "LinReceiveError",
        blf_core::ObjectType::LinSndError => "LinSendError",
        blf_core::ObjectType::LinSlvTimeout => "LinSlaveTimeout",
        blf_core::ObjectType::LinSchedModch => "LinSchedulerModeChange",
        blf_core::ObjectType::LinSynError => "LinSyncError",
        blf_core::ObjectType::LinBaudrate => "LinBaudrateEvent",
        blf_core::ObjectType::LinSleep => "LinSleepModeEvent",
        blf_core::ObjectType::LinWakeup => "LinWakeupEvent",
        blf_core::ObjectType::LinChecksumInfo => "LinChecksumInfo",
        blf_core::ObjectType::LinSpikeEvent => "LinSpikeEvent",
        blf_core::ObjectType::LinStatistic => "LinStatisticEvent",
        blf_core::ObjectType::LinMessage2 => "LinMessage2",
        blf_core::ObjectType::LinSndError2 => "LinSendError2",
        blf_core::ObjectType::LinSynError2 => "LinSyncError2",
        blf_core::ObjectType::LinCrcError2 => "LinCrcError2",
        blf_core::ObjectType::LinRcvError2 => "LinReceiveError2",
        blf_core::ObjectType::LinWakeup2 => "LinWakeupEvent2",
        blf_core::ObjectType::LinSpikeEvent2 => "LinSpikeEvent2",
        blf_core::ObjectType::LinLongDomSig => "LinLongDomSignalEvent",
        blf_core::ObjectType::LinLongDomSig2 => "LinLongDomSignalEvent2",
        blf_core::ObjectType::LinUnexpectedWakeup => "LinUnexpectedWakeup",
        blf_core::ObjectType::LinShortOrSlowResponse => "LinShortOrSlowResponse",
        blf_core::ObjectType::LinDisturbanceEvent => "LinDisturbanceEvent",
        blf_core::ObjectType::LinShortOrSlowResponse2 => "LinShortOrSlowResponse2",
        _ => "LinObject",
    }
}

fn field_value_to_py(py: Python<'_>, value: &blf_core::FieldValue) -> PyResult<PyObject> {
    let object = match value {
        blf_core::FieldValue::U8(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::U16(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::U32(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::U64(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::I32(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::F64(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::Bytes(value) => PyBytes::new(py, value).into_any(),
        blf_core::FieldValue::U16Vec(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::U64Vec(value) => value.into_pyobject(py)?.into_any(),
    };
    Ok(object.unbind())
}

#[pymodule]
fn _core(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(open, module)?)?;
    module.add_class::<PyFile>()?;
    Ok(())
}

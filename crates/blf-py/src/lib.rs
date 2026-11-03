use pyo3::exceptions::{PyException, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBytes, PyDict};
use std::sync::Arc;

pyo3::create_exception!(_core, BlfFormatError, PyException, "BLF format error.");
pyo3::create_exception!(
    _core,
    BlfTruncatedError,
    PyException,
    "BLF file is truncated."
);
pyo3::create_exception!(_core, SignalQueryError, PyException, "Signal query error.");
pyo3::create_exception!(
    _core,
    BlfCompressionError,
    PyException,
    "BLF compression error."
);
pyo3::create_exception!(_core, BlfIoError, PyException, "BLF I/O error.");
pyo3::create_exception!(
    _core,
    BlfLimitError,
    PyException,
    "BLF read limit exceeded."
);
pyo3::create_exception!(
    _core,
    BlfUnsupportedObjectError,
    PyException,
    "BLF object type is unsupported."
);
pyo3::create_exception!(
    _core,
    BlfUnsupportedVersionError,
    PyException,
    "BLF object header version is unsupported."
);

fn blf_error_to_py(error: blf_core::BlfError) -> PyErr {
    match error {
        blf_core::BlfError::Io(error) => BlfIoError::new_err(error.to_string()),
        blf_core::BlfError::Format(message) => BlfFormatError::new_err(message),
        blf_core::BlfError::Truncated | blf_core::BlfError::Incomplete => {
            BlfTruncatedError::new_err(error.to_string())
        }
        blf_core::BlfError::Compression(message) => BlfCompressionError::new_err(message),
        blf_core::BlfError::Limit {
            kind,
            limit,
            actual,
        } => BlfLimitError::new_err(format!(
            "BLF read limit exceeded for {kind}: limit={limit}, actual={actual}"
        )),
        blf_core::BlfError::UnsupportedObject(object_type) => {
            BlfUnsupportedObjectError::new_err(format!("unsupported object type: {object_type}"))
        }
        blf_core::BlfError::UnsupportedHeaderVersion(version) => {
            BlfUnsupportedVersionError::new_err(format!(
                "unsupported object header version: {version}"
            ))
        }
    }
}

fn database_error_to_py(error: blf_core::DatabaseError) -> PyErr {
    BlfFormatError::new_err(error.to_string())
}

fn query_error_to_py(error: blf_core::QueryError) -> PyErr {
    match error {
        blf_core::QueryError::Blf(error) => blf_error_to_py(error),
        other => SignalQueryError::new_err(other.to_string()),
    }
}

fn parse_bus(value: &str) -> PyResult<blf_core::BusKind> {
    match value.to_ascii_lowercase().as_str() {
        "can" => Ok(blf_core::BusKind::Can),
        "lin" => Ok(blf_core::BusKind::Lin),
        _ => Err(SignalQueryError::new_err("bus must be 'can' or 'lin'")),
    }
}

fn database_from_py(value: &Bound<'_, PyAny>) -> PyResult<blf_core::Database> {
    if let Ok(database) = value.extract::<PyRef<'_, PyDbcDatabase>>() {
        return Ok(blf_core::Database::Can(database.database.clone()));
    }
    if let Ok(database) = value.extract::<PyRef<'_, PyLdfDatabase>>() {
        return Ok(blf_core::Database::Lin(database.database.clone()));
    }
    Err(SignalQueryError::new_err(
        "database must be a DbcDatabase or LdfDatabase",
    ))
}

#[pyclass(name = "DbcDatabase")]
struct PyDbcDatabase {
    database: Arc<blf_core::DbcDatabase>,
}

#[pymethods]
impl PyDbcDatabase {
    #[staticmethod]
    fn from_file(path: &Bound<'_, PyAny>) -> PyResult<Self> {
        let path = path_to_string(path)?;
        blf_core::DbcDatabase::from_file(&path)
            .map(|database| Self {
                database: Arc::new(database),
            })
            .map_err(database_error_to_py)
    }

    #[staticmethod]
    fn from_str(source: String) -> PyResult<Self> {
        blf_core::DbcDatabase::from_str(&source)
            .map(|database| Self {
                database: Arc::new(database),
            })
            .map_err(database_error_to_py)
    }

    #[getter]
    fn source_path(&self) -> Option<&str> {
        self.database.source_path()
    }

    #[getter]
    fn version(&self) -> Option<&str> {
        self.database.version()
    }

    #[getter]
    fn attributes(&self, py: Python<'_>) -> PyResult<PyObject> {
        string_map_to_dict(py, self.database.attributes())
    }

    #[getter]
    fn scoped_attributes(&self, py: Python<'_>) -> PyResult<PyObject> {
        string_map_to_dict(py, self.database.scoped_attributes())
    }

    #[getter]
    fn messages(&self, py: Python<'_>) -> PyResult<Vec<PyObject>> {
        self.database
            .messages()
            .map(|message| dbc_message_to_dict(py, message))
            .collect()
    }

    #[getter]
    fn signal_names(&self) -> Vec<&str> {
        let mut names: Vec<_> = self.database.signal_names().collect();
        names.sort_unstable();
        names
    }

    #[pyo3(signature = (id, extended=false))]
    fn message_by_id(&self, py: Python<'_>, id: u32, extended: bool) -> PyResult<Option<PyObject>> {
        self.database
            .message_by_id(id, extended)
            .map(|message| dbc_message_to_dict(py, message))
            .transpose()
    }

    fn message_by_name(&self, py: Python<'_>, name: &str) -> PyResult<Option<PyObject>> {
        self.database
            .message_by_name(name)
            .map(|message| dbc_message_to_dict(py, message))
            .transpose()
    }

    fn signal(&self, py: Python<'_>, qualified_name: &str) -> PyResult<Option<PyObject>> {
        self.database
            .signal(qualified_name)
            .map(|(message, signal)| dbc_signal_to_dict(py, message, signal))
            .transpose()
    }

    #[getter]
    fn message_count(&self) -> usize {
        self.database.len()
    }

    fn __repr__(&self) -> String {
        format!("DbcDatabase(messages={})", self.database.len())
    }
}

fn string_map_to_dict<'py, I, K, V>(py: Python<'py>, values: I) -> PyResult<PyObject>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let dict = PyDict::new(py);
    for (name, value) in values {
        dict.set_item(name.as_ref(), value.as_ref())?;
    }
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn source_to_py(py: Python<'_>, source: Option<blf_core::SourceSpan>) -> PyResult<PyObject> {
    let Some(source) = source else {
        return Ok(py.None());
    };
    let dict = PyDict::new(py);
    dict.set_item("line", source.line)?;
    dict.set_item("column", source.column)?;
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn value_table_to_dict(py: Python<'_>, values: &blf_core::ValueTable) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    for (raw, text) in values.iter() {
        dict.set_item(raw, text)?;
    }
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn can_signal_to_dict(py: Python<'_>, signal: &blf_core::CanSignal) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("name", &signal.name)?;
    dict.set_item("start_bit", signal.start_bit)?;
    dict.set_item("bit_length", signal.bit_length)?;
    dict.set_item(
        "byte_order",
        match signal.byte_order {
            blf_core::ByteOrder::LittleEndian => "little_endian",
            blf_core::ByteOrder::BigEndian => "big_endian",
        },
    )?;
    dict.set_item(
        "signal_type",
        match signal.signal_type {
            blf_core::SignalType::Unsigned => "unsigned",
            blf_core::SignalType::Signed => "signed",
        },
    )?;
    dict.set_item("factor", signal.factor)?;
    dict.set_item("offset", signal.offset)?;
    dict.set_item("minimum", signal.minimum)?;
    dict.set_item("maximum", signal.maximum)?;
    dict.set_item("choices", value_table_to_dict(py, &signal.choices)?)?;
    dict.set_item(
        "multiplex",
        match &signal.multiplex {
            blf_core::MultiplexRole::None => "none".into_pyobject(py)?.into_any(),
            blf_core::MultiplexRole::Selector => "selector".into_pyobject(py)?.into_any(),
            blf_core::MultiplexRole::Branch(values) => values
                .iter()
                .copied()
                .collect::<Vec<_>>()
                .into_pyobject(py)?
                .into_any(),
        },
    )?;
    dict.set_item(
        "attributes",
        string_map_to_dict(py, signal.attributes.iter())?,
    )?;
    dict.set_item("comment", &signal.comment)?;
    dict.set_item("source", source_to_py(py, signal.source)?)?;
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn dbc_signal_to_dict(
    py: Python<'_>,
    message: &blf_core::DbcMessage,
    signal: &blf_core::CanSignal,
) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("message_id", message.id.id)?;
    dict.set_item("message_extended", message.id.extended)?;
    dict.set_item("message_name", &message.name)?;
    dict.set_item("signal", can_signal_to_dict(py, signal)?)?;
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn dbc_message_to_dict(py: Python<'_>, message: &blf_core::DbcMessage) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("id", message.id.id)?;
    dict.set_item("extended", message.id.extended)?;
    dict.set_item("name", &message.name)?;
    dict.set_item("dlc", message.dlc)?;
    dict.set_item("is_fd", message.is_fd)?;
    dict.set_item(
        "payload_kind",
        match message.payload_kind {
            blf_core::CanPayloadKind::Physical => "physical",
            blf_core::CanPayloadKind::LogicalContainer => "logical_container",
        },
    )?;
    dict.set_item(
        "signals",
        message
            .signals
            .iter()
            .map(|signal| can_signal_to_dict(py, signal))
            .collect::<PyResult<Vec<_>>>()?,
    )?;
    dict.set_item("comment", &message.comment)?;
    dict.set_item(
        "attributes",
        string_map_to_dict(py, message.attributes.iter())?,
    )?;
    dict.set_item("source", source_to_py(py, message.source)?)?;
    Ok(dict.into_pyobject(py)?.unbind().into())
}

#[pyclass(name = "LdfDatabase")]
struct PyLdfDatabase {
    database: Arc<blf_core::LdfDatabase>,
}

#[pymethods]
impl PyLdfDatabase {
    #[staticmethod]
    fn from_file(path: &Bound<'_, PyAny>) -> PyResult<Self> {
        let path = path_to_string(path)?;
        blf_core::LdfDatabase::from_file(&path)
            .map(|database| Self {
                database: Arc::new(database),
            })
            .map_err(database_error_to_py)
    }

    #[staticmethod]
    fn from_str(source: String) -> PyResult<Self> {
        blf_core::LdfDatabase::from_str(&source)
            .map(|database| Self {
                database: Arc::new(database),
            })
            .map_err(database_error_to_py)
    }

    #[getter]
    fn source_path(&self) -> Option<&str> {
        self.database.source_path()
    }

    #[getter]
    fn channel_name(&self) -> Option<&str> {
        self.database.channel_name()
    }

    #[getter]
    fn protocol_version(&self) -> Option<&str> {
        self.database.protocol_version()
    }

    #[getter]
    fn language_version(&self) -> Option<&str> {
        self.database.language_version()
    }

    #[getter]
    fn speed_kbps(&self) -> Option<f64> {
        self.database.speed_kbps()
    }

    #[getter]
    fn nodes(&self, py: Python<'_>) -> PyResult<Vec<PyObject>> {
        self.database
            .nodes()
            .map(|node| lin_node_to_dict(py, node))
            .collect()
    }

    #[getter]
    fn frames(&self, py: Python<'_>) -> PyResult<Vec<PyObject>> {
        self.database
            .frames()
            .map(|frame| lin_frame_to_dict(py, frame))
            .collect()
    }

    #[getter]
    fn schedules(&self, py: Python<'_>) -> PyResult<PyObject> {
        let schedules = PyDict::new(py);
        for (name, entries) in self.database.schedules() {
            let entries = entries
                .iter()
                .map(|entry| lin_schedule_entry_to_dict(py, entry))
                .collect::<PyResult<Vec<_>>>()?;
            schedules.set_item(name, entries)?;
        }
        Ok(schedules.into_pyobject(py)?.unbind().into())
    }

    #[getter]
    fn encodings(&self, py: Python<'_>) -> PyResult<PyObject> {
        let encodings = PyDict::new(py);
        for (name, encoding) in self.database.encodings() {
            encodings.set_item(name, lin_encoding_to_dict(py, encoding)?)?;
        }
        Ok(encodings.into_pyobject(py)?.unbind().into())
    }

    #[pyo3(signature = (id))]
    fn frame_by_id(&self, py: Python<'_>, id: u8) -> PyResult<Option<PyObject>> {
        self.database
            .frame_by_id(id)
            .map(|frame| lin_frame_to_dict(py, frame))
            .transpose()
    }

    fn frame_by_name(&self, py: Python<'_>, name: &str) -> PyResult<Option<PyObject>> {
        self.database
            .frame_by_name(name)
            .map(|frame| lin_frame_to_dict(py, frame))
            .transpose()
    }

    fn signal(&self, py: Python<'_>, qualified_name: &str) -> PyResult<Option<PyObject>> {
        self.database
            .signal(qualified_name)
            .map(|(frame, signal)| lin_signal_ref_to_dict(py, frame, signal))
            .transpose()
    }

    #[getter]
    fn frame_count(&self) -> usize {
        self.database.len()
    }

    #[getter]
    fn optional_sections(&self, py: Python<'_>) -> PyResult<Vec<PyObject>> {
        self.database
            .optional_sections()
            .map(|section| {
                let dict = PyDict::new(py);
                dict.set_item("name", &section.name)?;
                match section.source {
                    Some(source) => {
                        dict.set_item("line", source.line)?;
                        dict.set_item("column", source.column)?;
                    }
                    None => {
                        dict.set_item("line", py.None())?;
                        dict.set_item("column", py.None())?;
                    }
                }
                Ok(dict.into_pyobject(py)?.unbind().into())
            })
            .collect()
    }

    #[getter]
    fn dynamic_frames(&self, py: Python<'_>) -> PyResult<Vec<PyObject>> {
        self.database
            .dynamic_frames()
            .map(|dynamic| {
                let dict = PyDict::new(py);
                dict.set_item("name", &dynamic.name)?;
                dict.set_item(
                    "kind",
                    match dynamic.kind {
                        blf_core::LinDynamicFrameKind::Sporadic => "sporadic",
                        blf_core::LinDynamicFrameKind::EventTriggered => "event_triggered",
                    },
                )?;
                dict.set_item("trigger_frame_id", dynamic.trigger_frame_id)?;
                dict.set_item("frame_names", &dynamic.frame_names)?;
                dict.set_item("source", source_to_py(py, dynamic.source)?)?;
                Ok(dict.into_pyobject(py)?.unbind().into())
            })
            .collect()
    }

    fn __repr__(&self) -> String {
        format!("LdfDatabase(frames={})", self.database.len())
    }
}

fn lin_node_to_dict(py: Python<'_>, node: &blf_core::LinNode) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("name", &node.name)?;
    dict.set_item("configured_nad", node.configured_nad)?;
    dict.set_item("source", source_to_py(py, node.source)?)?;
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn lin_schedule_entry_to_dict(
    py: Python<'_>,
    entry: &blf_core::LinScheduleEntry,
) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("frame_name", &entry.frame_name)?;
    dict.set_item("delay_ms", entry.delay_ms)?;
    dict.set_item("source", source_to_py(py, entry.source)?)?;
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn lin_signal_to_dict(py: Python<'_>, signal: &blf_core::LinSignal) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("name", &signal.name)?;
    dict.set_item("start_bit", signal.start_bit)?;
    dict.set_item("bit_length", signal.bit_length)?;
    dict.set_item("initial_value", signal.initial_value)?;
    dict.set_item("factor", signal.factor)?;
    dict.set_item("offset", signal.offset)?;
    dict.set_item("minimum", signal.minimum)?;
    dict.set_item("maximum", signal.maximum)?;
    dict.set_item(
        "physical_ranges",
        signal
            .physical_ranges
            .iter()
            .map(|range| lin_physical_range_to_dict(py, range))
            .collect::<PyResult<Vec<_>>>()?,
    )?;
    dict.set_item("encoding", &signal.encoding)?;
    dict.set_item("choices", value_table_to_dict(py, &signal.choices)?)?;
    dict.set_item("source", source_to_py(py, signal.source)?)?;
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn lin_frame_to_dict(py: Python<'_>, frame: &blf_core::LinFrame) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("id", frame.id)?;
    dict.set_item("name", &frame.name)?;
    dict.set_item("dlc", frame.dlc)?;
    dict.set_item("publisher", &frame.publisher)?;
    dict.set_item(
        "checksum",
        match frame.checksum {
            blf_core::LinChecksum::Classic => "classic",
            blf_core::LinChecksum::Enhanced => "enhanced",
        },
    )?;
    dict.set_item(
        "signals",
        frame
            .signals
            .iter()
            .map(|signal| lin_signal_to_dict(py, signal))
            .collect::<PyResult<Vec<_>>>()?,
    )?;
    dict.set_item("source", source_to_py(py, frame.source)?)?;
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn lin_signal_ref_to_dict(
    py: Python<'_>,
    frame: &blf_core::LinFrame,
    signal: &blf_core::LinSignal,
) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("frame_id", frame.id)?;
    dict.set_item("frame_name", &frame.name)?;
    dict.set_item("signal", lin_signal_to_dict(py, signal)?)?;
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn lin_physical_range_to_dict(
    py: Python<'_>,
    range: &blf_core::LinPhysicalRange,
) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("minimum", range.minimum)?;
    dict.set_item("maximum", range.maximum)?;
    dict.set_item("factor", range.factor)?;
    dict.set_item("offset", range.offset)?;
    dict.set_item("unit", &range.unit)?;
    dict.set_item("text", &range.text)?;
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn lin_encoding_to_dict(py: Python<'_>, encoding: &blf_core::LinEncoding) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("name", &encoding.name)?;
    dict.set_item(
        "logical_values",
        value_table_to_dict(py, &encoding.logical_values)?,
    )?;
    dict.set_item(
        "physical_ranges",
        encoding
            .physical_ranges
            .iter()
            .map(|range| lin_physical_range_to_dict(py, range))
            .collect::<PyResult<Vec<_>>>()?,
    )?;
    dict.set_item("source", source_to_py(py, encoding.source)?)?;
    Ok(dict.into_pyobject(py)?.unbind().into())
}

fn path_to_string(path: &Bound<'_, PyAny>) -> PyResult<String> {
    if let Ok(path) = path.extract::<String>() {
        return Ok(path);
    }
    path.call_method0("__fspath__")?
        .extract::<String>()
        .map_err(|_| PyTypeError::new_err("path must be a string or path-like object"))
}

#[pyclass(name = "SignalQuery")]
struct PySignalQuery {
    query: Option<blf_core::SignalQuery>,
}

#[pymethods]
impl PySignalQuery {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<PyObject>> {
        let Some(mut query) = self.query.take() else {
            return Ok(None);
        };
        let item = py.allow_threads(|| query.next());
        self.query = Some(query);
        match item {
            Some(Ok(sample)) => sample_to_dict(py, &sample).map(Some),
            Some(Err(error)) => Err(query_error_to_py(error)),
            None => Ok(None),
        }
    }

    #[getter]
    fn strict(&self) -> bool {
        self.query
            .as_ref()
            .is_some_and(blf_core::SignalQuery::strict)
    }
}

#[pyfunction]
#[pyo3(signature = (path, strict=None, max_container_size=None, max_object_size=None, max_signature_scan=None, preserve_unknown_objects=None, skip_unknown_objects=None))]
fn open(
    path: String,
    strict: Option<bool>,
    max_container_size: Option<u64>,
    max_object_size: Option<u64>,
    max_signature_scan: Option<usize>,
    preserve_unknown_objects: Option<bool>,
    skip_unknown_objects: Option<bool>,
) -> PyResult<Py<PyFile>> {
    let mut options = blf_core::ReaderOptions::default();
    if let Some(value) = strict {
        options.strict = value;
    }
    if let Some(value) = max_container_size {
        options.max_uncompressed_container_size = value;
    }
    if let Some(value) = max_object_size {
        options.max_object_size = value;
    }
    if let Some(value) = max_signature_scan {
        options.max_object_signature_scan = value;
    }
    if let Some(value) = preserve_unknown_objects {
        options.preserve_unknown_objects = value;
    }
    if let Some(value) = skip_unknown_objects {
        options.skip_unknown_objects = value;
    }
    let reader =
        blf_core::File::open_with_reader_options(path, options).map_err(blf_error_to_py)?;
    Python::with_gil(|py| {
        Py::new(
            py,
            PyFile {
                reader,
                bindings: Vec::new(),
            },
        )
    })
}

#[pyclass(name = "File")]
struct PyFile {
    reader: blf_core::File,
    bindings: Vec<blf_core::DatabaseBinding>,
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

    fn good(&self) -> bool {
        self.reader.good()
    }

    fn eof(&self) -> bool {
        self.reader.eof()
    }

    #[getter]
    fn current_uncompressed_file_size(&self) -> u64 {
        self.reader.current_uncompressed_file_size
    }

    #[getter]
    fn current_object_count(&self) -> u32 {
        self.reader.current_object_count
    }

    #[getter]
    fn compression_level(&self) -> i32 {
        self.reader.compression_level
    }

    #[getter]
    fn max_container_size(&self) -> u64 {
        self.reader.options.max_uncompressed_container_size
    }

    #[getter]
    fn max_object_size(&self) -> u64 {
        self.reader.options.max_object_size
    }

    #[getter]
    fn max_signature_scan(&self) -> usize {
        self.reader.options.max_object_signature_scan
    }

    #[getter]
    fn preserve_unknown_objects(&self) -> bool {
        self.reader.options.preserve_unknown_objects
    }

    #[getter]
    fn skip_unknown_objects(&self) -> bool {
        self.reader.options.skip_unknown_objects
    }

    #[getter]
    fn write_restore_points(&self) -> bool {
        self.reader.write_restore_points
    }

    fn close(&mut self) {
        self.reader.close();
    }

    fn read(&mut self, py: Python<'_>) -> PyResult<Option<PyObject>> {
        let result = py.allow_threads(|| self.reader.read());
        match result.map_err(blf_error_to_py)? {
            Some(object) => object_to_dict(py, &object).map(Some),
            None => Ok(None),
        }
    }

    fn read_batch(&mut self, py: Python<'_>, max_objects: usize) -> PyResult<Vec<PyObject>> {
        let result = py.allow_threads(|| self.reader.read_batch(max_objects));
        let objects = result.map_err(blf_error_to_py)?;
        objects
            .iter()
            .map(|object| object_to_dict(py, object))
            .collect()
    }

    fn bind_database(
        &mut self,
        bus: String,
        channel: u16,
        database: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let bus = parse_bus(&bus)?;
        let database = database_from_py(database)?;
        let binding =
            blf_core::DatabaseBinding::new(bus, channel, database).map_err(database_error_to_py)?;
        self.bindings
            .retain(|existing| !(existing.bus() == bus && existing.channel() == channel));
        self.bindings.push(binding);
        Ok(())
    }

    #[pyo3(signature = (signal_name, bus, channel, strict=None))]
    fn signal(
        &self,
        py: Python<'_>,
        signal_name: String,
        bus: String,
        channel: u16,
        strict: Option<bool>,
    ) -> PyResult<Py<PySignalQuery>> {
        let bus_kind = parse_bus(&bus)?;
        let binding = self
            .bindings
            .iter()
            .find(|binding| binding.bus() == bus_kind && binding.channel() == channel)
            .cloned()
            .ok_or_else(|| {
                SignalQueryError::new_err(format!(
                    "no {bus_kind} database is bound to channel {channel}"
                ))
            })?;
        let query = self
            .reader
            .signal_query(binding, signal_name, strict.unwrap_or(false))
            .map_err(query_error_to_py)?;
        Py::new(py, PySignalQuery { query: Some(query) })
    }

    #[pyo3(signature = (signal_name, bus, channel, strict=None))]
    fn signals(
        &self,
        py: Python<'_>,
        signal_name: String,
        bus: String,
        channel: u16,
        strict: Option<bool>,
    ) -> PyResult<Py<PySignalQuery>> {
        let bus_kind = parse_bus(&bus)?;
        let bindings: Vec<blf_core::DatabaseBinding> = self
            .bindings
            .iter()
            .filter(|binding| binding.bus() == bus_kind && binding.channel() == channel)
            .cloned()
            .collect();
        if bindings.is_empty() {
            return Err(SignalQueryError::new_err(format!(
                "no {bus_kind} database is bound to channel {channel}"
            )));
        }
        let query = self
            .reader
            .aggregate_signal_query(bindings, signal_name, strict.unwrap_or(false))
            .map_err(query_error_to_py)?;
        Py::new(py, PySignalQuery { query: Some(query) })
    }
}

fn sample_to_dict(py: Python<'_>, sample: &blf_core::SignalSample) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("timestamp_ns", sample.timestamp_ns)?;
    dict.set_item("timestampNs", sample.timestamp_ns)?;
    dict.set_item("bus", sample.bus.as_str())?;
    dict.set_item("channel", sample.channel)?;
    dict.set_item("frame_id", sample.frame_id)?;
    dict.set_item("frameId", sample.frame_id)?;
    dict.set_item("message_name", &sample.message_name)?;
    dict.set_item("messageName", &sample.message_name)?;
    dict.set_item("signal_name", &sample.signal_name)?;
    dict.set_item("signalName", &sample.signal_name)?;
    dict.set_item("raw_value", sample.raw_value)?;
    dict.set_item("physical_value", sample.physical_value)?;
    dict.set_item("value_text", &sample.value_text)?;
    dict.set_item("status", sample.status.to_string())?;
    dict.set_item("error", &sample.error)?;
    dict.set_item("data", PyBytes::new(py, &sample.data))?;
    let value = match &sample.value {
        Some(blf_core::SampleValue::Text(value)) => value.clone().into_pyobject(py)?.into_any(),
        Some(blf_core::SampleValue::Physical(value)) => value.into_pyobject(py)?.into_any(),
        Some(blf_core::SampleValue::Raw(value)) => value.into_pyobject(py)?.into_any(),
        None => py.None().into_bound(py),
    };
    dict.set_item("value", value)?;
    Ok(dict.into_pyobject(py)?.into_any().unbind())
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
    dict.set_item("extensions", PyBytes::new(py, &stats.extensions))?;
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
    dict.set_item("objectType", object.object_type_raw())?;
    match object {
        blf_core::Object::Raw(value) => set_raw_header(&dict, &value.header)?,
        blf_core::Object::Structured(value) => set_raw_header(&dict, &value.header)?,
        _ => set_object_header(py, &dict, object_header(object))?,
    }
    dict.set_item("header", object_header_to_dict(py, object)?)?;

    match object {
        blf_core::Object::CanMessage(value) => {
            dict.set_item("class", "CanMessage")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("flags", value.flags)?;
            dict.set_item("dlc", value.dlc)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", PyBytes::new(py, &value.data))?;
            let fields = PyDict::new(py);
            fields.set_item("channel", value.channel)?;
            fields.set_item("flags", value.flags)?;
            fields.set_item("dlc", value.dlc)?;
            fields.set_item("id", value.id)?;
            fields.set_item("data", PyBytes::new(py, &value.data))?;
            dict.set_item("fields", fields)?;
        }
        blf_core::Object::CanMessage2(value) => {
            dict.set_item("class", "CanMessage2")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("flags", value.flags)?;
            dict.set_item("dlc", value.dlc)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", PyBytes::new(py, &value.data))?;
            dict.set_item("frameLength", value.frame_length)?;
            dict.set_item("bitCount", value.bit_count)?;
            let fields = PyDict::new(py);
            fields.set_item("channel", value.channel)?;
            fields.set_item("flags", value.flags)?;
            fields.set_item("dlc", value.dlc)?;
            fields.set_item("id", value.id)?;
            fields.set_item("data", PyBytes::new(py, &value.data))?;
            fields.set_item("frameLength", value.frame_length)?;
            fields.set_item("bitCount", value.bit_count)?;
            fields.set_item("reservedCanMessage1", value.reserved_can_message1)?;
            fields.set_item("reservedCanMessage2", value.reserved_can_message2)?;
            dict.set_item("fields", fields)?;
        }
        blf_core::Object::CanFdMessage(value) => {
            dict.set_item("class", "CanFdMessage")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("flags", value.flags)?;
            dict.set_item("canFdFlags", value.can_fd_flags)?;
            dict.set_item("dlc", value.dlc)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", PyBytes::new(py, &value.data))?;
            dict.set_item("frameLength", value.frame_length)?;
            let fields = PyDict::new(py);
            fields.set_item("channel", value.channel)?;
            fields.set_item("flags", value.flags)?;
            fields.set_item("dlc", value.dlc)?;
            fields.set_item("id", value.id)?;
            fields.set_item("frameLength", value.frame_length)?;
            fields.set_item("arbBitCount", value.arb_bit_count)?;
            fields.set_item("canFdFlags", value.can_fd_flags)?;
            fields.set_item("validDataBytes", value.valid_data_bytes)?;
            fields.set_item("reservedCanFdMessage1", value.reserved_can_fd_message1)?;
            fields.set_item("reservedCanFdMessage2", value.reserved_can_fd_message2)?;
            fields.set_item("data", PyBytes::new(py, &value.data))?;
            fields.set_item("reservedCanFdMessage3", value.reserved_can_fd_message3)?;
            dict.set_item("fields", fields)?;
        }
        blf_core::Object::CanFdMessage64(value) => {
            dict.set_item("class", "CanFdMessage64")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("dlc", value.dlc)?;
            dict.set_item("validDataBytes", value.valid_data_bytes)?;
            dict.set_item("canFdFlags", value.flags)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", PyBytes::new(py, &value.data))?;
            dict.set_item("frameLength", value.frame_length)?;
            let fields = PyDict::new(py);
            fields.set_item("channel", value.channel)?;
            fields.set_item("dlc", value.dlc)?;
            fields.set_item("validDataBytes", value.valid_data_bytes)?;
            fields.set_item("txCount", value.tx_count)?;
            fields.set_item("id", value.id)?;
            fields.set_item("frameLength", value.frame_length)?;
            fields.set_item("flags", value.flags)?;
            fields.set_item("btrCfgArb", value.btr_cfg_arb)?;
            fields.set_item("btrCfgData", value.btr_cfg_data)?;
            fields.set_item("timeOffsetBrsNs", value.time_offset_brs_ns)?;
            fields.set_item("timeOffsetCrcDelNs", value.time_offset_crc_del_ns)?;
            fields.set_item("bitCount", value.bit_count)?;
            fields.set_item("dir", value.dir)?;
            fields.set_item("extDataOffset", value.ext_data_offset)?;
            fields.set_item("crc", value.crc)?;
            fields.set_item("data", PyBytes::new(py, &value.data))?;
            if let Some(ext) = &value.ext_data {
                dict.set_item("extData", ext_data_to_dict(py, ext)?)?;
                fields.set_item("btrExtArb", ext.btr_ext_arb)?;
                fields.set_item("btrExtData", ext.btr_ext_data)?;
                fields.set_item(
                    "reservedCanFdExtFrameData",
                    PyBytes::new(py, &ext.reserved_can_fd_ext_frame_data),
                )?;
            } else {
                dict.set_item("extData", py.None())?;
                fields.set_item(
                    "reservedCanFdExtFrameData",
                    PyBytes::new(py, &value.reserved_can_fd_ext_frame_data),
                )?;
            }
            dict.set_item("fields", fields)?;
        }
        blf_core::Object::CanErrorFrame(value) => {
            dict.set_item("class", "CanErrorFrame")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("length", value.length)?;
            let fields = PyDict::new(py);
            fields.set_item("channel", value.channel)?;
            fields.set_item("length", value.length)?;
            fields.set_item("reservedCanErrorFrame", value.reserved_can_error_frame)?;
            dict.set_item("fields", fields)?;
        }
        blf_core::Object::CanErrorFrameExt(value) => {
            dict.set_item("class", "CanErrorFrameExt")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", PyBytes::new(py, &value.data))?;
            let fields = PyDict::new(py);
            fields.set_item("channel", value.channel)?;
            fields.set_item("length", value.length)?;
            fields.set_item("flags", value.flags)?;
            fields.set_item("ecc", value.ecc)?;
            fields.set_item("position", value.position)?;
            fields.set_item("dlc", value.dlc)?;
            fields.set_item(
                "reservedCanErrorFrameExt1",
                value.reserved_can_error_frame_ext1,
            )?;
            fields.set_item("frameLengthInNs", value.frame_length_in_ns)?;
            fields.set_item("id", value.id)?;
            fields.set_item("flagsExt", value.flags_ext)?;
            fields.set_item(
                "reservedCanErrorFrameExt2",
                value.reserved_can_error_frame_ext2,
            )?;
            fields.set_item("data", PyBytes::new(py, &value.data))?;
            dict.set_item("fields", fields)?;
        }
        blf_core::Object::CanFdErrorFrame64(value) => {
            dict.set_item("class", "CanFdErrorFrame64")?;
            dict.set_item("channel", value.channel)?;
            dict.set_item("id", value.id)?;
            dict.set_item("data", PyBytes::new(py, &value.data))?;
            let fields = PyDict::new(py);
            fields.set_item("channel", value.channel)?;
            fields.set_item("dlc", value.dlc)?;
            fields.set_item("validDataBytes", value.valid_data_bytes)?;
            fields.set_item("ecc", value.ecc)?;
            fields.set_item("flags", value.flags)?;
            fields.set_item("errorCodeExt", value.error_code_ext)?;
            fields.set_item("extFlags", value.ext_flags)?;
            fields.set_item("extDataOffset", value.ext_data_offset)?;
            fields.set_item(
                "reservedCanFdErrorFrame1",
                value.reserved_can_fd_error_frame1,
            )?;
            fields.set_item("id", value.id)?;
            fields.set_item("frameLength", value.frame_length)?;
            fields.set_item("btrCfgArb", value.btr_cfg_arb)?;
            fields.set_item("btrCfgData", value.btr_cfg_data)?;
            fields.set_item("timeOffsetBrsNs", value.time_offset_brs_ns)?;
            fields.set_item("timeOffsetCrcDelNs", value.time_offset_crc_del_ns)?;
            fields.set_item("crc", value.crc)?;
            fields.set_item("errorPosition", value.error_position)?;
            fields.set_item(
                "reservedCanFdErrorFrame2",
                value.reserved_can_fd_error_frame2,
            )?;
            fields.set_item("data", PyBytes::new(py, &value.data))?;
            if let Some(ext) = &value.ext_data {
                dict.set_item("extData", ext_data_to_dict(py, ext)?)?;
                fields.set_item("btrExtArb", ext.btr_ext_arb)?;
                fields.set_item("btrExtData", ext.btr_ext_data)?;
                fields.set_item(
                    "reservedCanFdExtFrameData",
                    PyBytes::new(py, &ext.reserved_can_fd_ext_frame_data),
                )?;
            } else {
                dict.set_item("extData", py.None())?;
                fields.set_item(
                    "reservedCanFdExtFrameData",
                    PyBytes::new(py, &value.reserved_can_fd_ext_frame_data),
                )?;
            }
            dict.set_item("fields", fields)?;
        }
        blf_core::Object::AppText(value) => {
            dict.set_item("class", "AppText")?;
            dict.set_item("source", value.source)?;
            dict.set_item("text", PyBytes::new(py, &value.text))?;
            let fields = PyDict::new(py);
            fields.set_item("source", value.source)?;
            fields.set_item("reservedAppText1", value.reserved_app_text1)?;
            fields.set_item("textLength", value.text_length)?;
            fields.set_item("reservedAppText2", value.reserved_app_text2)?;
            fields.set_item("text", PyBytes::new(py, &value.text))?;
            dict.set_item("fields", fields)?;
        }
        blf_core::Object::RestorePointContainer(value) => {
            dict.set_item("class", "RestorePointContainer")?;
            dict.set_item("dataLength", value.data_length)?;
            dict.set_item("data", PyBytes::new(py, &value.data))?;
            if let Some(restore_points) = &value.restore_points {
                dict.set_item("restorePoints", restore_points_to_dict(py, restore_points)?)?;
            } else {
                dict.set_item("restorePoints", py.None())?;
            }
            let fields = PyDict::new(py);
            fields.set_item(
                "reservedRestorePointContainer",
                PyBytes::new(py, &value.reserved_restore_point_container),
            )?;
            fields.set_item("dataLength", value.data_length)?;
            fields.set_item("data", PyBytes::new(py, &value.data))?;
            if let Some(restore_points) = &value.restore_points {
                fields.set_item("restorePoints", restore_points_to_dict(py, restore_points)?)?;
            } else {
                fields.set_item("restorePoints", py.None())?;
            }
            dict.set_item("fields", fields)?;
        }
        blf_core::Object::Lin(value) => {
            dict.set_item("class", lin_class_name(value.object_type))?;
            dict.set_item("objectType", value.object_type.as_u32())?;
            let fields = PyDict::new(py);
            for (name, field) in &value.fields {
                let field_value = field_value_to_py(py, field)?;
                dict.set_item(name, &field_value)?;
                fields.set_item(name, field_value)?;
            }
            dict.set_item("fields", fields)?;
            dict.set_item("extensions", PyBytes::new(py, &value.extensions))?;
        }
        blf_core::Object::Structured(value) => {
            dict.set_item("class", &value.class_name)?;
            dict.set_item("className", &value.class_name)?;
            dict.set_item("data", PyBytes::new(py, &value.data))?;
            let header_size = raw_header_size(&value.header);
            dict.set_item("payload", PyBytes::new(py, &value.data[header_size..]))?;
            dict.set_item("extensions", PyBytes::new(py, &value.extensions))?;
            let fields = PyDict::new(py);
            for (name, field) in &value.fields {
                let field_value = field_value_to_py(py, field)?;
                dict.set_item(name, &field_value)?;
                fields.set_item(name, field_value)?;
            }
            dict.set_item("fields", fields)?;
        }
        blf_core::Object::Raw(value) => {
            dict.set_item("class", &value.class_name)?;
            dict.set_item("className", &value.class_name)?;
            dict.set_item("payload", PyBytes::new(py, &value.payload))?;
            dict.set_item("data", PyBytes::new(py, &value.data))?;
            set_raw_header(&dict, &value.header)?;
        }
    }

    let raw_data = match object {
        blf_core::Object::CanMessage(value) => &value.raw_data,
        blf_core::Object::CanMessage2(value) => &value.raw_data,
        blf_core::Object::CanFdMessage(value) => &value.raw_data,
        blf_core::Object::CanFdMessage64(value) => &value.raw_data,
        blf_core::Object::CanErrorFrame(value) => &value.raw_data,
        blf_core::Object::CanErrorFrameExt(value) => &value.raw_data,
        blf_core::Object::CanFdErrorFrame64(value) => &value.raw_data,
        blf_core::Object::AppText(value) => &value.raw_data,
        blf_core::Object::RestorePointContainer(value) => &value.raw_data,
        blf_core::Object::Lin(value) => &value.raw_data,
        blf_core::Object::Structured(value) => &value.data,
        blf_core::Object::Raw(value) => &value.data,
    };
    dict.set_item("rawData", PyBytes::new(py, raw_data))?;
    if let Some(timestamp_ns) = blf_core::object_timestamp_ns(object) {
        dict.set_item("timestampNs", timestamp_ns)?;
    }
    if let Some(extensions) = typed_extensions(object) {
        dict.set_item("extensions", PyBytes::new(py, extensions))?;
    }

    // Structured fields are decoded in one place in the Rust core. Promote
    // them to the object mapping as well as retaining the `fields` mapping so
    // every decoded Rust field supports direct Python attribute access.
    promote_fields(&dict)?;

    Ok(dict.into_pyobject(py)?.into_any().unbind())
}

fn promote_fields(dict: &Bound<'_, PyDict>) -> PyResult<()> {
    let Some(fields) = dict.get_item("fields")? else {
        return Ok(());
    };
    let fields = fields.downcast::<PyDict>()?;
    for (key, value) in fields.iter() {
        dict.set_item(key, value)?;
    }
    Ok(())
}

fn ext_data_to_dict(py: Python<'_>, ext: &blf_core::CanFdExtFrameData) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("btrExtArb", ext.btr_ext_arb)?;
    dict.set_item("btrExtData", ext.btr_ext_data)?;
    dict.set_item(
        "reservedCanFdExtFrameData",
        PyBytes::new(py, &ext.reserved_can_fd_ext_frame_data),
    )?;
    Ok(dict.into_pyobject(py)?.into_any().unbind())
}

fn restore_points_to_dict(
    py: Python<'_>,
    restore_points: &blf_core::RestorePoints,
) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("objectSize", restore_points.object_size)?;
    dict.set_item("objectInterval", restore_points.object_interval)?;
    let points = restore_points
        .restore_points
        .iter()
        .map(|point| {
            let item = PyDict::new(py);
            item.set_item("timeStamp", point.time_stamp)?;
            item.set_item("compressedFilePosition", point.compressed_file_position)?;
            item.set_item("uncompressedFileOffset", point.uncompressed_file_offset)?;
            item.set_item("unknownRestorePoint", point.unknown_restore_point)?;
            Ok::<_, PyErr>(item.into_pyobject(py)?.into_any().unbind())
        })
        .collect::<PyResult<Vec<_>>>()?;
    dict.set_item("restorePoints", points)?;
    Ok(dict.into_pyobject(py)?.into_any().unbind())
}

fn typed_extensions(object: &blf_core::Object) -> Option<&[u8]> {
    match object {
        blf_core::Object::CanMessage(value) => Some(value.raw_data.get(48..).unwrap_or_default()),
        blf_core::Object::CanFdMessage(value) => {
            Some(value.raw_data.get(120..).unwrap_or_default())
        }
        blf_core::Object::CanErrorFrame(value) => {
            let fixed_size = if value.length > 0 { 40 } else { 36 };
            Some(value.raw_data.get(fixed_size..).unwrap_or_default())
        }
        blf_core::Object::AppText(value) => {
            let fixed_size = 48usize.saturating_add(value.text.len());
            Some(value.raw_data.get(fixed_size..).unwrap_or_default())
        }
        blf_core::Object::RestorePointContainer(value) => {
            let fixed_size = 48usize.saturating_add(value.data.len());
            Some(value.raw_data.get(fixed_size..).unwrap_or_default())
        }
        blf_core::Object::CanMessage2(_)
        | blf_core::Object::CanFdMessage64(_)
        | blf_core::Object::CanErrorFrameExt(_)
        | blf_core::Object::CanFdErrorFrame64(_)
        | blf_core::Object::Lin(_)
        | blf_core::Object::Structured(_)
        | blf_core::Object::Raw(_) => None,
    }
}

fn raw_header_size(header: &blf_core::RawObjectHeader) -> usize {
    match header {
        blf_core::RawObjectHeader::Object(value) => usize::from(value.base.header_size),
        blf_core::RawObjectHeader::Object2(value) => usize::from(value.base.header_size),
        blf_core::RawObjectHeader::Var(value) => usize::from(value.base.header_size),
        blf_core::RawObjectHeader::Base(value) => usize::from(value.header_size),
    }
}

fn object_header(object: &blf_core::Object) -> &blf_core::ObjectHeader {
    match object {
        blf_core::Object::CanMessage(value) => &value.header,
        blf_core::Object::CanMessage2(value) => &value.header,
        blf_core::Object::CanFdMessage(value) => &value.header,
        blf_core::Object::CanFdMessage64(value) => &value.header,
        blf_core::Object::CanErrorFrame(value) => &value.header,
        blf_core::Object::CanErrorFrameExt(value) => &value.header,
        blf_core::Object::CanFdErrorFrame64(value) => &value.header,
        blf_core::Object::AppText(value) => &value.header,
        blf_core::Object::RestorePointContainer(value) => &value.header,
        blf_core::Object::Lin(value) => &value.header,
        blf_core::Object::Structured(_) => unreachable!("structured objects use set_raw_header"),
        blf_core::Object::Raw(_) => unreachable!("raw objects use set_raw_header"),
    }
}

fn set_raw_header(dict: &Bound<'_, PyDict>, header: &blf_core::RawObjectHeader) -> PyResult<()> {
    match header {
        blf_core::RawObjectHeader::Object(value) => set_object_header_values(
            dict,
            &value.base,
            value.object_flags,
            Some(value.client_index),
            value.object_version,
            Some(value.object_time_stamp),
            None,
        )?,
        blf_core::RawObjectHeader::Object2(value) => set_object_header_values(
            dict,
            &value.base,
            value.object_flags,
            None,
            value.object_version,
            Some(value.object_time_stamp),
            Some((
                value.time_stamp_status,
                value.reserved_object_header,
                value.original_time_stamp,
            )),
        )?,
        blf_core::RawObjectHeader::Var(value) => {
            set_object_header_values(
                dict,
                &value.base,
                value.object_flags,
                None,
                value.object_version,
                Some(value.object_time_stamp),
                None,
            )?;
            dict.set_item("objectStaticSize", value.object_static_size)?;
        }
        blf_core::RawObjectHeader::Base(value) => {
            dict.set_item("signature", value.signature)?;
            dict.set_item("headerSize", value.header_size)?;
            dict.set_item("objectSize", value.object_size)?;
            dict.set_item("headerVersion", value.header_version)?;
            dict.set_item("objectType", value.object_type.as_u32())?;
            dict.set_item("rawObjectType", value.raw_object_type)?;
        }
    }
    Ok(())
}

fn set_object_header_values(
    dict: &Bound<'_, PyDict>,
    base: &blf_core::ObjectHeaderBase,
    object_flags: u32,
    client_index: Option<u16>,
    object_version: u16,
    object_time_stamp: Option<u64>,
    header2: Option<(u8, u8, u64)>,
) -> PyResult<()> {
    dict.set_item("signature", base.signature)?;
    dict.set_item("headerSize", base.header_size)?;
    dict.set_item("objectSize", base.object_size)?;
    dict.set_item("headerVersion", base.header_version)?;
    dict.set_item("objectType", base.object_type.as_u32())?;
    dict.set_item("rawObjectType", base.raw_object_type)?;
    dict.set_item("objectFlags", object_flags)?;
    dict.set_item("objectVersion", object_version)?;
    if let Some(value) = client_index {
        dict.set_item("clientIndex", value)?;
    }
    if let Some(value) = object_time_stamp {
        dict.set_item("objectTimeStamp", value)?;
    }
    if let Some((status, reserved, original)) = header2 {
        dict.set_item("timeStampStatus", status)?;
        dict.set_item("reservedObjectHeader", reserved)?;
        dict.set_item("originalTimeStamp", original)?;
    }
    Ok(())
}

fn set_object_header(
    _py: Python<'_>,
    dict: &Bound<'_, PyDict>,
    header: &blf_core::ObjectHeader,
) -> PyResult<()> {
    dict.set_item("signature", header.base.signature)?;
    dict.set_item("headerSize", header.base.header_size)?;
    dict.set_item("objectSize", header.base.object_size)?;
    dict.set_item("headerVersion", header.base.header_version)?;
    dict.set_item("objectType", header.base.object_type.as_u32())?;
    dict.set_item("rawObjectType", header.base.raw_object_type)?;
    dict.set_item("objectFlags", header.object_flags)?;
    dict.set_item("clientIndex", header.client_index)?;
    dict.set_item("objectVersion", header.object_version)?;
    dict.set_item("objectTimeStamp", header.object_time_stamp)?;
    Ok(())
}

fn object_header_to_dict(py: Python<'_>, object: &blf_core::Object) -> PyResult<PyObject> {
    match object {
        blf_core::Object::CanMessage(value) => object_header_to_dict_typed(py, &value.header),
        blf_core::Object::CanMessage2(value) => object_header_to_dict_typed(py, &value.header),
        blf_core::Object::CanFdMessage(value) => object_header_to_dict_typed(py, &value.header),
        blf_core::Object::CanFdMessage64(value) => object_header_to_dict_typed(py, &value.header),
        blf_core::Object::CanErrorFrame(value) => object_header_to_dict_typed(py, &value.header),
        blf_core::Object::CanErrorFrameExt(value) => object_header_to_dict_typed(py, &value.header),
        blf_core::Object::CanFdErrorFrame64(value) => {
            object_header_to_dict_typed(py, &value.header)
        }
        blf_core::Object::AppText(value) => object_header_to_dict_typed(py, &value.header),
        blf_core::Object::RestorePointContainer(value) => {
            object_header_to_dict_typed(py, &value.header)
        }
        blf_core::Object::Lin(value) => object_header_to_dict_typed(py, &value.header),
        blf_core::Object::Structured(value) => raw_header_to_dict(py, &value.header),
        blf_core::Object::Raw(value) => raw_header_to_dict(py, &value.header),
    }
}

fn object_header_to_dict_typed(
    py: Python<'_>,
    header: &blf_core::ObjectHeader,
) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    set_object_header(py, &dict, header)?;
    dict.set_item("base", base_header_to_dict(py, &header.base)?)?;
    Ok(dict.into_pyobject(py)?.into_any().unbind())
}

fn raw_header_to_dict(py: Python<'_>, header: &blf_core::RawObjectHeader) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    set_raw_header(&dict, header)?;
    if let Some(base) = raw_header_base(header) {
        dict.set_item("base", base_header_to_dict(py, base)?)?;
    }
    Ok(dict.into_pyobject(py)?.into_any().unbind())
}

fn raw_header_base(header: &blf_core::RawObjectHeader) -> Option<&blf_core::ObjectHeaderBase> {
    match header {
        blf_core::RawObjectHeader::Object(value) => Some(&value.base),
        blf_core::RawObjectHeader::Object2(value) => Some(&value.base),
        blf_core::RawObjectHeader::Var(value) => Some(&value.base),
        blf_core::RawObjectHeader::Base(_) => None,
    }
}

fn base_header_to_dict(py: Python<'_>, base: &blf_core::ObjectHeaderBase) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("signature", base.signature)?;
    dict.set_item("headerSize", base.header_size)?;
    dict.set_item("headerVersion", base.header_version)?;
    dict.set_item("objectSize", base.object_size)?;
    dict.set_item("objectType", base.object_type.as_u32())?;
    dict.set_item("rawObjectType", base.raw_object_type)?;
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
        blf_core::FieldValue::I16(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::U32(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::U64(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::I32(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::I64(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::F64(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::Bytes(value) => PyBytes::new(py, value).into_any(),
        blf_core::FieldValue::String(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::U16Vec(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::U32Vec(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::U64Vec(value) => value.into_pyobject(py)?.into_any(),
        blf_core::FieldValue::I64Vec(value) => value.into_pyobject(py)?.into_any(),
    };
    Ok(object.unbind())
}

#[pymodule]
fn _core(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(open, module)?)?;
    module.add_class::<PyFile>()?;
    module.add_class::<PyDbcDatabase>()?;
    module.add_class::<PyLdfDatabase>()?;
    module.add_class::<PySignalQuery>()?;
    module.add("BlfFormatError", module.py().get_type::<BlfFormatError>())?;
    module.add("BlfIoError", module.py().get_type::<BlfIoError>())?;
    module.add("BlfLimitError", module.py().get_type::<BlfLimitError>())?;
    module.add(
        "BlfTruncatedError",
        module.py().get_type::<BlfTruncatedError>(),
    )?;
    module.add(
        "BlfCompressionError",
        module.py().get_type::<BlfCompressionError>(),
    )?;
    module.add(
        "BlfUnsupportedObjectError",
        module.py().get_type::<BlfUnsupportedObjectError>(),
    )?;
    module.add(
        "BlfUnsupportedVersionError",
        module.py().get_type::<BlfUnsupportedVersionError>(),
    )?;
    module.add(
        "SignalQueryError",
        module.py().get_type::<SignalQueryError>(),
    )?;
    Ok(())
}

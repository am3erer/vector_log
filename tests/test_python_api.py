import os
import struct

import pytest

from vector_log.blf_rs.classes import SignalSample, object_from_data
from vector_log.blf_rs import (
    DbcDatabase,
    File,
    LdfDatabase,
    SignalQueryError,
    _core,
    open,
)
from vector_log.blf_rs.export import (
    objects_to_csv,
    objects_to_jsonl,
    objects_to_numpy,
    samples_to_arrow,
    samples_to_numpy,
)


DBC = r'''
VERSION "1.0"
NS_ :
BS_:
BU_: ECU

BO_ 256 FirstStatus: 2 ECU
 SG_ Speed : 0|8@1+ (0.5,-10) [-10|117.5] "km/h" ECU

BO_ 257 SecondStatus: 2 ECU
 SG_ Speed : 0|8@1+ (1,100) [100|355] "km/h" ECU

BO_ 258 ShortStatus: 2 ECU
 SG_ Wide : 0|16@1+ (1,0) [0|65535] "" ECU

VAL_ 256 Speed 1 "Moving";
'''


DBC_METADATA = r'''
VERSION "1.0"
NS_ :
BS_:
BU_: ECU

BO_ 512 Multiplexed: 8 ECU
 SG_ Mux M : 0|4@1+ (1,0) [0|15] "" ECU
 SG_ Common : 8|8@1+ (1,0) [0|255] "" ECU
 SG_ Branch m1 : 16|8@1+ (0.5,-1) [-1|126.5] "km/h" ECU
 SG_ BranchMulti m2 : 24|8@1+ (1,10) [10|265] "" ECU

CM_ BO_ 512 "multiplexed message";
CM_ SG_ 512 Common "common signal";
VAL_ 512 Common 0 "Idle" 1 "Active";
BA_DEF_ BO_ "CycleTime" INT 0 10000;
BA_DEF_ SG_ "SignalType" STRING ;
BA_DEF_ BU_ "NodeRole" STRING ;
BA_DEF_ EV_ "Environment" STRING ;
BA_ "CycleTime" BO_ 512 10;
BA_ "SignalType" SG_ 512 Common "status";
BA_ "NodeRole" BU_ ECU "controller";
BA_ "Environment" EV_ "Ambient" "outside";
SG_MUL_VAL_ 512 Branch Mux 1-3,5,7-8;
'''


LDF = r'''
LIN_description_file;
LIN_protocol_version = "2.2";
LIN_language_version = "2.2";
LIN_speed = 19.2 kbps;
Channel_name = "TEST_LIN";

Nodes {
  Master: MasterNode, 5 ms, 0.1 ms ;
  Slaves: SlaveA ;
}

Signals {
  State: 8, 0, SlaveA, MasterNode ;
}

Frames {
  Status: 1, SlaveA, 1 {
    State, 0 ;
  }
}
'''


LDF_METADATA = LDF[:-1] + r'''

Schedule_tables {
  Normal {
    Status delay 10 ms ;
  }
}

Signal_encoding_types {
  StateEncoding {
    logical_value, 0, "Off" ;
    logical_value, 1, "On" ;
  }
}

Signal_representation {
  StateEncoding: State ;
}
'''


LDF_MULTI_RANGE = LDF[:-1] + r'''

Signal_encoding_types {
  ValueEncoding {
    physical_value, 0, 10, 1, 0, "unit" ;
    physical_value, 11, 20, 0.5, 5, "unit" ;
  }
}

Signal_representation {
  ValueEncoding: State ;
}
'''


def _u16(value):
    return struct.pack("<H", value)


def _u32(value):
    return struct.pack("<I", value)


def _u64(value):
    return struct.pack("<Q", value)


def _file_statistics(uncompressed_size, object_count):
    output = bytearray()
    output += _u32(0x47474F4C)
    output += _u32(144)
    output += _u32(4_080_200)
    output += bytes((1, 1, 1, 0))
    output += _u64(0)
    output += _u64(uncompressed_size)
    output += _u32(object_count)
    output += _u32(1)
    for value in (2026, 11, 1, 1, 0, 0, 0, 0):
        output += _u16(value)
    for value in (2026, 11, 1, 1, 0, 0, 0, 0):
        output += _u16(value)
    output += _u64(0)
    output += b"\0" * 64
    return bytes(output)


def _object_header(object_type, object_size, timestamp):
    return struct.pack(
        "<IHHIIIHHQ",
        0x4A424F4C,
        32,
        1,
        object_size,
        object_type,
        2,
        0,
        0,
        timestamp,
    )


def _can_message(channel, frame_id, timestamp, data):
    output = bytearray(_object_header(1, 48, timestamp))
    output += _u16(channel)
    output += bytes((0, len(data)))
    output += _u32(frame_id)
    output += bytes(data)
    output += b"\0" * (48 - len(output))
    return bytes(output)


def _lin_message(channel, frame_id, timestamp, data):
    output = bytearray(_object_header(11, 56, timestamp))
    output += _u16(channel)
    output += bytes((frame_id, len(data)))
    output += bytes(data)
    output += b"\0" * (8 - len(data))
    output += bytes((0, 0, 0, 0))
    frame_id &= 0x3F
    p0 = (frame_id ^ (frame_id >> 1) ^ (frame_id >> 2) ^ (frame_id >> 4)) & 0x01
    p1 = (~((frame_id >> 1) ^ (frame_id >> 3) ^ (frame_id >> 4) ^ (frame_id >> 5))) & 0x01
    protected_id = frame_id | (p0 << 6) | (p1 << 7)
    checksum = protected_id
    for byte in data:
        checksum += byte
        if checksum > 0xFF:
            checksum = (checksum & 0xFF) + 1
    output += _u16((~checksum) & 0xFF)
    output += bytes((0, 0))
    output += _u32(0)
    assert len(output) == 56
    return bytes(output)


def _write_blf(path):
    inner = b"".join(
        (
            _can_message(1, 0x100, 10, (1, 0)),
            _can_message(1, 0x101, 20, (7, 0)),
            _can_message(1, 0x102, 30, (0xAA,)),
        )
    )
    container_size = 32 + len(inner)
    container = bytearray()
    container += struct.pack(
        "<IHHIIHHIII",
        0x4A424F4C,
        16,
        1,
        container_size,
        10,
        0,
        0,
        0,
        len(inner),
        0,
    )
    container += inner
    container += b"\0" * (container_size % 4)
    path.write_bytes(_file_statistics(len(inner), 3) + container)


def _write_strict_blf(path):
    inner = b"".join(
        (
            _can_message(1, 0x102, 10, (0xAA,)),
            _can_message(1, 0x102, 20, (0x01, 0x00)),
        )
    )
    container_size = 32 + len(inner)
    container = bytearray()
    container += struct.pack(
        "<IHHIIHHIII",
        0x4A424F4C,
        16,
        1,
        container_size,
        10,
        0,
        0,
        0,
        len(inner),
        0,
    )
    container += inner
    container += b"\0" * (container_size % 4)
    path.write_bytes(_file_statistics(len(inner), 2) + container)


def _write_lin_blf(path):
    inner = _lin_message(3, 1, 40, (0x2A,))
    container_size = 32 + len(inner)
    container = bytearray()
    container += struct.pack(
        "<IHHIIHHIII",
        0x4A424F4C,
        16,
        1,
        container_size,
        10,
        0,
        0,
        0,
        len(inner),
        0,
    )
    container += inner
    container += b"\0" * (container_size % 4)
    path.write_bytes(_file_statistics(len(inner), 1) + container)


def test_native_database_parsers_and_path_bindings(tmp_path):
    dbc_path = tmp_path / "vehicle.dbc"
    ldf_path = tmp_path / "body.ldf"
    dbc_path.write_text(DBC, encoding="utf-8")
    ldf_path.write_text(LDF, encoding="utf-8")

    dbc = DbcDatabase.from_str(DBC)
    ldf = LdfDatabase.from_file(ldf_path)

    assert dbc.source_path == "<memory>"
    assert dbc.message_count == 3
    assert ldf.source_path == str(ldf_path)
    assert ldf.frame_count == 1
    assert ldf.optional_sections == []
    assert repr(dbc) == "DbcDatabase(messages=3)"
    assert repr(ldf) == "LdfDatabase(frames=1)"


def test_python_dbc_metadata_view_exposes_all_native_fields():
    dbc = DbcDatabase.from_str(DBC_METADATA)

    assert dbc.version == "1.0"
    assert dbc.attributes == {}
    assert dbc.signal_names == [
        "Multiplexed.Branch",
        "Multiplexed.BranchMulti",
        "Multiplexed.Common",
        "Multiplexed.Mux",
    ]
    assert dbc.scoped_attributes == {
        "BU_:ECU:NodeRole": "controller",
        "EV_:Ambient:Environment": "outside",
    }

    message = dbc.message_by_id(512)
    assert message == dbc.message_by_name("Multiplexed")
    assert message["id"] == 512
    assert message["extended"] is False
    assert message["name"] == "Multiplexed"
    assert message["dlc"] == 8
    assert message["is_fd"] is False
    assert message["payload_kind"] == "physical"
    assert message["comment"] == "multiplexed message"
    assert message["attributes"] == {"CycleTime": "10"}
    assert message["source"]["line"] > 0

    signals = {signal["name"]: signal for signal in message["signals"]}
    assert signals["Mux"]["multiplex"] == "selector"
    assert signals["Common"]["choices"] == {0: "Idle", 1: "Active"}
    assert signals["Common"]["attributes"] == {"SignalType": "status"}
    assert signals["Common"]["comment"] == "common signal"
    assert signals["Branch"]["multiplex"] == [1, 2, 3, 5, 7, 8]
    assert signals["Branch"]["factor"] == 0.5
    assert signals["Branch"]["offset"] == -1.0
    assert signals["Branch"]["minimum"] == -1.0
    assert signals["Branch"]["maximum"] == 126.5
    assert signals["Branch"]["byte_order"] == "little_endian"
    assert signals["Branch"]["signal_type"] == "unsigned"

    qualified = dbc.signal("Multiplexed.Common")
    assert qualified["message_id"] == 512
    assert qualified["message_name"] == "Multiplexed"
    assert qualified["signal"] == signals["Common"]
    assert dbc.signal("Missing.Signal") is None
    assert dbc.message_by_id(512, extended=True) is None


def test_python_ldf_metadata_view_exposes_all_native_fields():
    ldf = LdfDatabase.from_str(LDF_METADATA)

    assert ldf.channel_name == "TEST_LIN"
    assert ldf.protocol_version == "2.2"
    assert ldf.language_version == "2.2"
    assert ldf.speed_kbps == 19.2
    assert [(node["name"], node["configured_nad"]) for node in ldf.nodes] == [
        ("MasterNode", None),
        ("SlaveA", None),
    ]
    assert all(node["source"]["line"] > 0 for node in ldf.nodes)

    frame = ldf.frame_by_id(1)
    assert frame == ldf.frame_by_name("Status")
    assert frame["id"] == 1
    assert frame["name"] == "Status"
    assert frame["dlc"] == 1
    assert frame["publisher"] == "SlaveA"
    assert frame["checksum"] == "enhanced"
    assert frame["source"]["line"] > 0

    signal = frame["signals"][0]
    assert signal["name"] == "State"
    assert signal["start_bit"] == 0
    assert signal["bit_length"] == 8
    assert signal["initial_value"] == 0
    assert signal["factor"] == 1.0
    assert signal["offset"] == 0.0
    assert signal["encoding"] == "StateEncoding"
    assert signal["choices"] == {0: "Off", 1: "On"}

    assert set(ldf.schedules) == {"Normal"}
    assert ldf.schedules["Normal"][0]["frame_name"] == "Status"
    assert ldf.schedules["Normal"][0]["delay_ms"] == 10
    assert ldf.schedules["Normal"][0]["source"]["line"] > 0
    assert ldf.encodings["StateEncoding"]["name"] == "StateEncoding"
    assert ldf.encodings["StateEncoding"]["logical_values"] == {0: "Off", 1: "On"}
    assert ldf.encodings["StateEncoding"]["physical_ranges"] == []
    assert ldf.encodings["StateEncoding"]["source"]["line"] > 0
    assert ldf.optional_sections == []

    qualified = ldf.signal("Status.State")
    assert qualified["frame_id"] == 1
    assert qualified["frame_name"] == "Status"
    assert qualified["signal"] == signal
    assert ldf.signal("Missing.Signal") is None
    assert ldf.frame_by_id(2) is None


def test_python_ldf_signal_view_preserves_multiple_physical_ranges():
    ldf = LdfDatabase.from_str(LDF_MULTI_RANGE)

    signal = ldf.frame_by_id(1)["signals"][0]
    assert signal["encoding"] == "ValueEncoding"
    assert signal["factor"] == 1.0
    assert signal["offset"] == 0.0
    assert signal["physical_ranges"] == [
        {
            "minimum": 0.0,
            "maximum": 10.0,
            "factor": 1.0,
            "offset": 0.0,
            "unit": "unit",
            "text": None,
        },
        {
            "minimum": 11.0,
            "maximum": 20.0,
            "factor": 0.5,
            "offset": 5.0,
            "unit": "unit",
            "text": None,
        },
    ]
    assert ldf.encodings["ValueEncoding"]["physical_ranges"] == signal["physical_ranges"]


def test_python_ldf_dynamic_frame_metadata_is_structured():
    source = """
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { State: 1, 0, ECU, ECU ; }
Frames { Status: 1, ECU, 1 { State, 0 ; } }
Sporadic_frames { StatusGroup { Status ; } }
Event_triggered_frames { Trigger { Status, Status ; } }
"""
    ldf = LdfDatabase.from_str(source)

    assert ldf.dynamic_frames == [
        {
            "name": "StatusGroup",
            "kind": "sporadic",
            "trigger_frame_id": None,
            "frame_names": ["Status"],
            "source": {"line": 6, "column": 19},
        },
        {
            "name": "Trigger",
            "kind": "event_triggered",
            "trigger_frame_id": None,
            "frame_names": ["Status", "Status"],
            "source": {"line": 7, "column": 26},
        },
    ]


def test_python_signal_query_preserves_context_and_cursor(tmp_path):
    blf_path = tmp_path / "signals.blf"
    dbc_path = tmp_path / "vehicle.dbc"
    _write_blf(blf_path)
    dbc_path.write_text(DBC, encoding="utf-8")

    reader = open(blf_path, databases={("can", 1): dbc_path})
    query = reader.signal("FirstStatus.Speed", bus="can", channel=1)
    assert query.strict is False

    sample = next(query)
    assert sample.timestamp_ns == 10
    assert sample.bus == "can"
    assert sample.channel == 1
    assert sample.frame_id == 0x100
    assert sample.message_name == "FirstStatus"
    assert sample.signal_name == "Speed"
    assert sample.raw_value == 1
    assert sample.physical_value == -9.5
    assert sample.value == "Moving"
    assert sample.value_text == "Moving"
    assert sample.status == "ok"
    assert sample.error is None
    assert sample.data == b"\x01\x00"

    # Query cursors are independent from the normal reader cursor.
    first_object = reader.read()
    assert first_object.frame_id == 0x100
    assert first_object.payload == b"\x01\x00"


def test_aggregate_query_returns_each_matching_message(tmp_path):
    blf_path = tmp_path / "aggregate.blf"
    dbc_path = tmp_path / "vehicle.dbc"
    _write_blf(blf_path)
    dbc_path.write_text(DBC, encoding="utf-8")

    reader = open(blf_path, databases={("can", 1): dbc_path})
    samples = reader.signals("Speed", bus="can", channel=1).collect()

    assert [(sample.message_name, sample.timestamp_ns) for sample in samples] == [
        ("FirstStatus", 10),
        ("SecondStatus", 20),
    ]
    assert [sample.raw_value for sample in samples] == [1, 7]
    assert [sample.physical_value for sample in samples] == [-9.5, 107.0]


def test_tolerant_and_strict_queries_keep_decode_errors_explicit(tmp_path):
    blf_path = tmp_path / "short.blf"
    dbc_path = tmp_path / "vehicle.dbc"
    _write_blf(blf_path)
    dbc_path.write_text(DBC, encoding="utf-8")

    tolerant_reader = open(blf_path, databases={("can", 1): dbc_path})
    sample = tolerant_reader.signal("ShortStatus.Wide", bus="can", channel=1).collect()[0]
    assert sample.status == "payload_too_short"
    assert sample.raw_value is None
    assert sample.physical_value is None
    assert sample.value is None
    assert sample.error
    assert sample.data == b"\xAA"

    strict_reader = open(blf_path, databases={("can", 1): dbc_path})
    strict_query = strict_reader.signal(
        "ShortStatus.Wide", bus="can", channel=1, strict=True
    )
    with pytest.raises(SignalQueryError, match="payload is too short"):
        next(strict_query)


def test_strict_query_stops_after_first_decode_error(tmp_path):
    blf_path = tmp_path / "strict-stop.blf"
    dbc_path = tmp_path / "vehicle.dbc"
    _write_strict_blf(blf_path)
    dbc_path.write_text(DBC, encoding="utf-8")

    reader = open(blf_path, databases={("can", 1): dbc_path})
    query = reader.signal("ShortStatus.Wide", bus="can", channel=1, strict=True)
    with pytest.raises(SignalQueryError, match="payload is too short"):
        next(query)
    with pytest.raises(StopIteration):
        next(query)


def test_signal_materialization_helpers_preserve_nullable_columns(tmp_path):
    try:
        import numpy as np
    except ImportError:
        pytest.skip("NumPy is not installed")
    blf_path = tmp_path / "materialize.blf"
    dbc_path = tmp_path / "vehicle.dbc"
    _write_blf(blf_path)
    dbc_path.write_text(DBC, encoding="utf-8")

    reader = open(blf_path, databases={("can", 1): dbc_path})
    columns = reader.signal("FirstStatus.Speed", bus="can", channel=1).to_numpy()
    assert columns["timestamp_ns"].tolist() == [10]
    assert columns["raw_value"].tolist() == [1]
    assert columns["raw_value"].dtype == np.int64
    assert not columns["raw_value"].mask[0]
    assert columns["physical_value"].tolist() == [-9.5]
    assert columns["data"].tolist() == [b"\x01\x00"]

    try:
        table = reader.signal("FirstStatus.Speed", bus="can", channel=1).to_arrow()
    except ImportError:
        table = None
    if table is not None:
        assert table.column_names == [
            "timestamp_ns",
            "bus",
            "channel",
            "frame_id",
            "message_name",
            "signal_name",
            "raw_value",
            "physical_value",
            "value",
            "value_text",
            "status",
            "error",
            "data",
        ]
        assert table["raw_value"].to_pylist() == [1]


def test_sample_numpy_timestamps_preserve_unsigned_dtype():
    try:
        import numpy as np
    except ImportError:
        return
    row = SignalSample(
        {
            "timestamp_ns": 2**63 + 1,
            "bus": "can",
            "channel": 1,
            "frame_id": 1,
            "message_name": "Status",
            "signal_name": "Speed",
            "raw_value": 1,
            "physical_value": 1.0,
            "value": 1.0,
            "value_text": None,
            "status": "ok",
            "error": None,
            "data": b"",
        }
    )
    columns = samples_to_numpy([row])
    assert columns["timestamp_ns"].dtype == np.uint64
    assert columns["timestamp_ns"][0] == 2**63 + 1


def test_sample_numpy_raw_values_preserve_large_integers_and_nullability():
    try:
        import numpy as np
    except ImportError:
        return
    rows = [
        SignalSample(
            {
                "timestamp_ns": 1,
                "bus": "can",
                "channel": 1,
                "frame_id": 1,
                "message_name": "Status",
                "signal_name": "Value",
                "raw_value": 2**63 - 1,
                "physical_value": 1.0,
                "value": 1.0,
                "value_text": None,
                "status": "ok",
                "error": None,
                "data": b"",
            }
        ),
        SignalSample(
            {
                "timestamp_ns": 2,
                "bus": "can",
                "channel": 1,
                "frame_id": 1,
                "message_name": "Status",
                "signal_name": "Value",
                "raw_value": None,
                "physical_value": None,
                "value": None,
                "value_text": None,
                "status": "decode_error",
                "error": "missing",
                "data": b"",
            }
        ),
    ]
    columns = samples_to_numpy(rows)
    assert columns["raw_value"].dtype == np.int64
    assert columns["raw_value"][0] == 2**63 - 1
    assert columns["raw_value"].mask.tolist() == [False, True]
    assert np.isnan(columns["physical_value"][1])


def test_sample_numpy_raw_values_preserve_unsigned_64_bit_values():
    try:
        import numpy as np
    except ImportError:
        return
    row = SignalSample(
        {
            "timestamp_ns": 1,
            "bus": "can",
            "channel": 1,
            "frame_id": 1,
            "message_name": "Status",
            "signal_name": "Counter",
            "raw_value": 2**64 - 1,
            "physical_value": 1.0,
            "value": 1.0,
            "value_text": None,
            "status": "ok",
            "error": None,
            "data": b"",
        }
    )
    columns = samples_to_numpy([row])
    assert columns["raw_value"].dtype == np.uint64
    assert columns["raw_value"][0] == 2**64 - 1

    try:
        table = samples_to_arrow([row])
    except ImportError:
        table = None
    if table is not None:
        assert str(table["raw_value"].type) == "uint64"
        assert table["raw_value"].to_pylist() == [2**64 - 1]


def test_object_numpy_timestamps_preserve_unsigned_dtype():
    try:
        import numpy as np
    except ImportError:
        return
    obj = object_from_data(
        {
            "class": "CanMessage",
            "className": "CanMessage",
            "objectType": 1,
            "timestampNs": 2**63 + 1,
            "channel": 1,
            "id": 1,
            "data": b"",
        }
    )
    columns = objects_to_numpy([obj])
    assert columns["timestamp_ns"].dtype == np.uint64
    assert columns["timestamp_ns"][0] == 2**63 + 1


def test_object_numpy_timestamps_use_mask_for_missing_values():
    try:
        import numpy as np
    except ImportError:
        return
    obj = object_from_data(
        {
            "class": "CanMessage",
            "className": "CanMessage",
            "objectType": 1,
            "channel": 1,
            "id": 1,
            "data": b"",
        }
    )
    columns = objects_to_numpy([obj])
    assert columns["timestamp_ns"].dtype == np.uint64
    assert columns["timestamp_ns"].mask.tolist() == [True]


def test_signal_query_samples_is_lazy_iterable_view(tmp_path):
    blf_path = tmp_path / "samples-property.blf"
    dbc_path = tmp_path / "vehicle.dbc"
    _write_blf(blf_path)
    dbc_path.write_text(DBC, encoding="utf-8")
    reader = open(blf_path, databases={("can", 1): dbc_path})
    query = reader.signal("FirstStatus.Speed", bus="can", channel=1)
    assert query.samples is query
    assert next(query.samples).signal_name == "Speed"


def test_arrow_materialization_normalizes_mixed_display_values():
    rows = [
        SignalSample(
            {
                "timestamp_ns": 1,
                "bus": "can",
                "channel": 1,
                "frame_id": 1,
                "message_name": "Status",
                "signal_name": "Mode",
                "raw_value": 1,
                "physical_value": 1.0,
                "value": "Active",
                "value_text": "Active",
                "status": "ok",
                "error": None,
                "data": b"\x01",
            }
        ),
        SignalSample(
            {
                "timestamp_ns": 2,
                "bus": "can",
                "channel": 1,
                "frame_id": 1,
                "message_name": "Status",
                "signal_name": "Mode",
                "raw_value": 2,
                "physical_value": 2.0,
                "value": 2.0,
                "value_text": None,
                "status": "ok",
                "error": None,
                "data": b"\x02",
            }
        ),
    ]
    try:
        table = samples_to_arrow(rows)
    except ImportError:
        return
    assert table["value"].to_pylist() == ["Active", "2.0"]
    assert table["value"].type == table["value_text"].type
    assert table["physical_value"].to_pylist() == [1.0, 2.0]


def test_database_binding_requires_explicit_protocol_channel(tmp_path):
    blf_path = tmp_path / "binding.blf"
    _write_blf(blf_path)
    reader = open(blf_path)

    with pytest.raises(SignalQueryError, match="no can database is bound to channel 1"):
        reader.signal("FirstStatus.Speed", bus="can", channel=1)
    with pytest.raises(SignalQueryError, match="no can database is bound to channel 1"):
        reader.signals("Speed", bus="can", channel=1)

    reader.bind_database("can", 1, DbcDatabase.from_str(DBC))
    with pytest.raises(SignalQueryError, match="not present"):
        reader.signal("Missing.Signal", bus="can", channel=1)


def test_file_supports_constructible_and_reopenable_compatibility_api(tmp_path):
    blf_path = tmp_path / "file-api.blf"
    _write_blf(blf_path)

    reader = File()
    assert not reader.is_open()
    with pytest.raises(ValueError, match="BLF file is not open"):
        reader.read()
    with pytest.raises(ValueError, match="BLF file is not open"):
        reader.read_batch(1)
    with pytest.raises(ValueError, match="BLF file is not open"):
        reader.signal("Status.Speed", bus="can", channel=1)
    with pytest.raises(ValueError, match="BLF file is not open"):
        reader.signals("Speed", bus="can", channel=1)
    assert reader.open(blf_path, "r") is reader
    assert reader.is_open()
    assert reader.read().class_name == "CanMessage"
    reader.close()
    assert not reader.is_open()
    assert reader.read() is None
    reader = File(blf_path)
    reader.bind_database("can", 1, DbcDatabase.from_str(DBC))
    reader.close()
    with pytest.raises(ValueError, match="BLF file is not open"):
        reader.signal("FirstStatus.Speed", bus="can", channel=1)
    with pytest.raises(ValueError, match="BLF file is not open"):
        reader.signals("Speed", bus="can", channel=1)

    reader = File(blf_path)
    assert reader.read().class_name == "CanMessage"
    reader.close()

    native = _core.open(os.fspath(blf_path))
    reader = File(native)
    assert reader.read().class_name == "CanMessage"
    reader.close()


def test_database_binding_replaces_existing_protocol_channel_binding(tmp_path):
    blf_path = tmp_path / "binding-replacement.blf"
    _write_blf(blf_path)
    replacement = r'''
BO_ 256 Replacement: 2 ECU
 SG_ Other : 0|8@1+ (1,0) [0|255] "" ECU
'''
    reader = open(blf_path)
    reader.bind_database("can", 1, DbcDatabase.from_str(DBC))
    reader.bind_database("can", 1, DbcDatabase.from_str(replacement))

    with pytest.raises(SignalQueryError, match="not present"):
        reader.signal("FirstStatus.Speed", bus="can", channel=1)
    sample = next(reader.signal("Replacement.Other", bus="can", channel=1))
    assert sample.raw_value == 1


def test_python_lin_signal_query_uses_ldf_binding(tmp_path):
    blf_path = tmp_path / "lin.blf"
    ldf_path = tmp_path / "body.ldf"
    _write_lin_blf(blf_path)
    ldf_path.write_text(LDF, encoding="utf-8")

    reader = open(blf_path, databases={("lin", 3): ldf_path})
    sample = next(reader.signal("Status.State", bus="lin", channel=3))

    assert sample.timestamp_ns == 40
    assert sample.bus == "lin"
    assert sample.channel == 3
    assert sample.frame_id == 1
    assert sample.message_name == "Status"
    assert sample.signal_name == "State"
    assert sample.raw_value == 0x2A
    assert sample.physical_value == 0x2A
    assert sample.value == 0x2A
    assert sample.status == "ok"
    assert sample.error is None
    assert sample.data == b"\x2A"


def test_nested_restore_points_support_attribute_access():
    obj = object_from_data(
        {
            "class": "RestorePointContainer",
            "className": "RestorePointContainer",
            "objectType": 115,
            "restorePoints": {
                "objectSize": 32,
                "objectInterval": 4,
                "restorePoints": [
                    {
                        "timeStamp": 10,
                        "compressedFilePosition": 20,
                        "uncompressedFileOffset": 30,
                        "unknownRestorePoint": 40,
                    }
                ],
            },
        }
    )

    assert obj.restore_points.object_size == 32
    assert obj.restore_points.object_interval == 4
    assert obj.restore_points.restore_points[0].time_stamp == 10
    assert obj.restore_points.restore_points[0].unknown_restore_point == 40


def test_lin_direction_and_payload_aliases():
    obj = object_from_data(
        {
            "class": "LinMessage2",
            "className": "LinMessage2",
            "objectType": 57,
            "id": 1,
            "dlc": 2,
            "dir": 1,
            "data": b"\xFD\xFE\xFF",
        }
    )

    assert obj.direction == "Tx"
    assert obj.frame_id == 1
    assert obj.payload == b"\xFD\xFE"
    assert obj.data_bytes == b"\xFD\xFE\xFF"


def test_numpy_export_is_columnar_and_payloads_are_length_delimited():
    objects = [
        object_from_data(
            {
                "class": "CanMessage",
                "className": "CanMessage",
                "objectType": 1,
                "timestampNs": 10,
                "channel": 1,
                "id": 0x123,
                "dlc": 2,
                "data": b"\x01\x02\xFF",
            }
        )
    ]
    columns = objects_to_numpy(objects, data_width=4)

    assert columns["timestamp_ns"].tolist() == [10]
    assert columns["frame_id"].tolist() == [0x123]
    assert columns["data_length"].tolist() == [2]
    assert columns["data"].shape == (1, 4)
    assert columns["data"][0].tolist() == [1, 2, 0, 0]


def test_structured_protocol_payload_aliases():
    cases = (
        ("EthernetFrame", "payLoad", "payLoadLength"),
        ("WlanFrame", "frameData", "frameLength"),
        ("AfdxFrame", "payLoad", "payLoadLength"),
        ("MostPkt", "pktData", "pktDataLength"),
    )
    for class_name, data_name, length_name in cases:
        obj = object_from_data(
            {
                "class": class_name,
                "className": class_name,
                "objectType": 1,
                data_name: b"abc",
                length_name: 2,
            }
        )
        assert obj.payload == b"ab"
        assert obj.payload_length == 2


def test_can_fd_flags_do_not_leak_into_classic_can():
    classic = object_from_data(
        {
            "class": "CanMessage",
            "className": "CanMessage",
            "objectType": 1,
            "flags": 0x10,
        }
    )
    can_fd = object_from_data(
        {
            "class": "CanFdMessage",
            "className": "CanFdMessage",
            "objectType": 100,
            "canFdFlags": 0x3010,
        }
    )
    can_fd_64 = object_from_data(
        {
            "class": "CanFdMessage64",
            "className": "CanFdMessage64",
            "objectType": 101,
            "flags": 0x3010,
        }
    )

    assert classic.can_fd_flags is None
    assert not classic.is_remote_frame
    assert can_fd.is_remote_frame
    assert can_fd.is_fd
    assert can_fd.bit_rate_switch
    assert can_fd_64.is_remote_frame
    assert can_fd_64.is_fd
    assert can_fd_64.bit_rate_switch


def test_filtering_and_text_exports_are_stable():
    objects = [
        object_from_data(
            {
                "class": "CanMessage",
                "className": "CanMessage",
                "objectType": 1,
                "timestampNs": 100,
                "channel": 2,
                "id": 0x123,
                "data": b"\x01\x02",
            }
        )
    ]

    csv_text = objects_to_csv(objects)
    jsonl_text = objects_to_jsonl(objects)
    assert "timestamp_ns,object_type,class_name" in csv_text
    assert "01 02" in csv_text
    assert '"frame_id":291' in jsonl_text
    assert '"data":"01 02"' in jsonl_text


def test_filter_value_helpers_accept_names_and_numeric_ids():
    obj = object_from_data(
        {
            "class": "CanMessage",
            "className": "CanMessage",
            "objectType": 1,
            "timestampNs": 100,
            "channel": 2,
            "id": 0x123,
        }
    )
    from vector_log.blf_rs import _filter_values, _matches_object_type

    values = _filter_values(["CanMessage", 1])
    assert _matches_object_type(obj, values)

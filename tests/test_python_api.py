from vector_log.blf_rs.classes import object_from_data
from vector_log.blf_rs.export import (
    objects_to_csv,
    objects_to_jsonl,
    objects_to_numpy,
)


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

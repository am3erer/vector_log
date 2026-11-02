"""Optional cantools bridge for decoded CAN frames."""

from . import open


def _payload(frame):
    return bytes(frame.payload)


def iter_decoded(
    path,
    database,
    *,
    strict=None,
    decode_choices=False,
    skip_remote_frames=True,
    allow_unknown_frames=False,
):
    """Yield ``(frame_id, decoded)`` pairs for CAN frames in a BLF file.

    ``database`` is an already loaded cantools database. Keeping the dependency
    optional lets the BLF parser remain usable without installing cantools.
    """
    with open(path, strict=strict) as reader:
        for frame in reader.can_messages():
            if skip_remote_frames and frame.is_remote_frame:
                continue
            frame_id = frame.frame_id
            try:
                decoded = database.decode_message(
                    frame_id,
                    _payload(frame),
                    decode_choices=decode_choices,
                )
            except KeyError:
                if allow_unknown_frames:
                    continue
                raise ValueError(f"CAN ID 0x{frame_id:X} is missing from the database") from None
            yield frame_id, decoded


__all__ = ["iter_decoded"]

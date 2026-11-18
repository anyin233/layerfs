"""Make the pinned Debian x86-64 UML interpreter relative to its working directory."""

import struct
import sys
from pathlib import Path


def relocate(path):
    image = bytearray(path.read_bytes())
    elf = struct.Struct("<16sHHIQQQIHHHHHH")
    ident, _, machine, _, _, headers, _, _, _, header_size, count, *_ = elf.unpack_from(
        image
    )
    program = struct.Struct("<IIQQQQQQ")
    if (
        ident[:7] != b"\x7fELF\x02\x01\x01"
        or machine != 62
        or header_size != program.size
    ):
        raise SystemExit("expected a Linux x86-64 ELF with standard program headers")

    interpreters = []
    for index in range(count):
        kind, _, offset, _, _, length, _, _ = program.unpack_from(
            image, headers + index * header_size
        )
        if kind == 3:  # PT_INTERP
            interpreters.append((offset, length))
    if len(interpreters) != 1:
        raise SystemExit("expected one PT_INTERP segment in linux.uml")
    offset, length = interpreters[0]
    if image[offset : offset + length] != b"/lib64/ld-linux-x86-64.so.2\0":
        raise SystemExit("unexpected PT_INTERP in linux.uml")
    image[offset : offset + length] = b"lib/ld-linux-x86-64.so.2\0".ljust(length, b"\0")
    path.write_bytes(image)


if __name__ == "__main__":
    relocate(Path(sys.argv[1]))

"""Verify that the allocator diagnostic imports libc malloc rather than defining it."""
import hashlib
import json
from pathlib import Path
import sys

from elftools.elf.elffile import ELFFile

binary = Path(sys.argv[1])
with binary.open('rb') as stream:
    elf = ELFFile(stream)
    dynamic = elf.get_section_by_name('.dynsym')
    if dynamic is None:
        raise SystemExit('System allocator diagnostic requires dynamic allocator symbols')
    selected = {symbol.name: symbol['st_shndx'] for symbol in dynamic.iter_symbols()
                if symbol.name in {'malloc', 'calloc', 'realloc', 'free'}}
if any(section != 'SHN_UNDEF' for section in selected.values()):
    raise SystemExit('Diagnostic binary defines an allocator symbol instead of importing libc')
if not {'malloc', 'calloc'}.issubset(selected):
    raise SystemExit('Diagnostic binary does not expose the expected libc allocator imports')
print(json.dumps({'allocator': 'system', 'dynamicSymbols': selected,
                  'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'passed': True}, indent=2))

"""Inspect actual ELF version needs against the CentOS 8 system ABI."""
import hashlib
import json
from pathlib import Path
import sys

from elftools.elf.elffile import ELFFile


def inspect(binary):
    with binary.open('rb') as stream:
        elf = ELFFile(stream)
        if elf['e_machine'] != 'EM_X86_64' or elf.elfclass != 64:
            raise ValueError('Expected a Linux x86-64 executable')
        dynamic = elf.get_section_by_name('.dynamic')
        dependencies = [entry.needed for entry in dynamic.iter_tags()
                        if entry.entry.d_tag == 'DT_NEEDED'] if dynamic else []
        section = elf.get_section_by_name('.gnu.version_r')
        versions = sorted({entry.name for _, entries in section.iter_versions()
                           for entry in entries}) if section else []
    limits = {'GLIBC': (2, 28), 'GLIBCXX': (3, 4, 25), 'CXXABI': (1, 3, 11)}
    violations = []
    maxima = {}
    for namespace, maximum in limits.items():
        selected = [name for name in versions if name.startswith(namespace + '_')]
        numeric = []
        for name in selected:
            suffix = name[len(namespace) + 1:]
            if not all(part.isdecimal() for part in suffix.split('.')):
                violations.append(name)
                continue
            version = tuple(map(int, suffix.split('.')))
            numeric.append(version)
            if version > maximum:
                violations.append(name)
        maxima[namespace] = '.'.join(map(str, max(numeric))) if numeric else None
    violations.extend(name for name in dependencies
                      if name.startswith(('libc++.', 'libc++abi.', 'libunwind.')))
    return {'binary': binary.name, 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
            'needed': dependencies, 'versionNeeds': versions, 'maximum': maxima,
            'violations': violations, 'passed': not violations}


if __name__ == '__main__':
    result = inspect(Path(sys.argv[1]))
    print(json.dumps(result, indent=2))
    raise SystemExit(0 if result['passed'] else 1)

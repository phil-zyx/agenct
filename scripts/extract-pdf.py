#!/usr/bin/env python3
"""Bounded local PDF-to-text conversion. No stored uploads, logs, model calls or shell."""
import os
import resource
import shutil
import subprocess
import sys
import tempfile

MAX_PDF = 2 * 1024 * 1024
MAX_TEXT = 256 * 1024


def limits():
    resource.setrlimit(resource.RLIMIT_CPU, (5, 5))
    resource.setrlimit(resource.RLIMIT_AS, (512 * 1024 * 1024,) * 2)
    resource.setrlimit(resource.RLIMIT_FSIZE, (MAX_TEXT + 1,) * 2)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


def extract(data):
    if len(data) > MAX_PDF or not data.startswith(b'%PDF-'):
        raise ValueError('PDF must be valid and at most 2 MiB')
    converter = shutil.which('pdftotext')
    if not converter:
        raise ValueError('PDF extraction requires poppler-utils on the Hub host')
    with tempfile.TemporaryDirectory(prefix='agenct-pdf-') as directory:
        with tempfile.TemporaryFile(dir=directory) as output:
            result = subprocess.run([converter, '-enc', 'UTF-8', '-nopgbrk', '-', '-'],
                                    input=data, stdout=output, stderr=subprocess.DEVNULL,
                                    cwd=directory, env={'PATH': '/usr/bin:/bin', 'HOME': directory, 'LANG': 'C.UTF-8'},
                                    timeout=8, preexec_fn=limits, check=False)
            output.seek(0)
            text = output.read(MAX_TEXT + 1)
    if result.returncode or len(text) > MAX_TEXT:
        raise ValueError('PDF conversion failed or extracted text exceeds 256 KiB')
    decoded = text.decode('utf-8').strip()
    if not decoded or '\x00' in decoded:
        raise ValueError('PDF contains no usable text; scanned PDFs need OCR before uploading')
    return decoded


if __name__ == '__main__':
    try:
        print(extract(sys.stdin.buffer.read(MAX_PDF + 1)), end='')
    except (ValueError, UnicodeError, OSError, subprocess.SubprocessError):
        # Never echo parser diagnostics or document contents.
        sys.exit(1)

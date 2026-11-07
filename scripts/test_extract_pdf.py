import importlib.util
from pathlib import Path
import shutil
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('extract_pdf', Path(__file__).with_name('extract-pdf.py'))
pdf = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pdf)


def document(text='Private PDF material'):
    stream = f'BT /F1 12 Tf 72 720 Td ({text}) Tj ET'.encode()
    objects = [b'<< /Type /Catalog /Pages 2 0 R >>',
               b'<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
               b'<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>',
               b'<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
               b'<< /Length ' + str(len(stream)).encode() + b' >>\nstream\n' + stream + b'\nendstream']
    result = b'%PDF-1.4\n'; offsets = [0]
    for index, obj in enumerate(objects, 1):
        offsets.append(len(result)); result += str(index).encode() + b' 0 obj\n' + obj + b'\nendobj\n'
    start = len(result)
    result += b'xref\n0 6\n0000000000 65535 f \n'
    result += b''.join(f'{n:010d} 00000 n \n'.encode() for n in offsets[1:])
    return result + f'trailer\n<< /Root 1 0 R /Size 6 >>\nstartxref\n{start}\n%%EOF\n'.encode()


class PdfTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which('pdftotext'), 'install poppler-utils for real PDF extraction')
    def test_real_text_pdf_and_invalid_document(self):
        self.assertEqual(pdf.extract(document()), 'Private PDF material')
        for value in (b'%PDF-not-a-document', document('')):
            with self.assertRaises(ValueError): pdf.extract(value)

    def test_bounds_and_missing_converter(self):
        for value in (b'not PDF', b'%PDF-' + b'a' * pdf.MAX_PDF):
            with self.assertRaises(ValueError): pdf.extract(value)
        with patch.object(pdf.shutil, 'which', return_value=None):
            with self.assertRaisesRegex(ValueError, 'poppler-utils'): pdf.extract(document())

    def test_converter_timeout_is_not_silently_accepted(self):
        with patch.object(pdf.shutil, 'which', return_value='/usr/bin/pdftotext'), patch.object(pdf.subprocess, 'run', side_effect=pdf.subprocess.TimeoutExpired('pdftotext', 8)):
            with self.assertRaises(pdf.subprocess.TimeoutExpired): pdf.extract(document())

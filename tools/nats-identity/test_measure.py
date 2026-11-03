"""Reject incomplete performance evidence rather than publish partial medians."""
import unittest

import measure


def output():
    lines = []
    for name in sorted(measure.WORKLOADS):
        metrics = '100 ns/op 200 B/op 3 allocs/op'
        if name == 'BenchmarkIdentityConnz':
            metrics += ' 1024 client_bytes 80 identity_state_bytes 60000 response_bytes'
        lines.append(f'{name}-4 10 {metrics}')
    return '\n'.join(lines) + '\nPASS\n'


class MeasurementTests(unittest.TestCase):
    def test_every_workload_and_retained_memory_are_present(self):
        rows = measure.parse_sample(output())
        self.assertEqual(rows['BenchmarkIdentityConnz']['client_bytes'], 1024)

    def test_missing_workload_duplicate_and_empty_iteration_fail(self):
        lines = output().splitlines()
        for broken in ['\n'.join(lines[1:]), output() + lines[0],
                       output().replace('-4 10 ', '-4 0 ', 1)]:
            with self.subTest(broken=broken):
                with self.assertRaises(ValueError):
                    measure.parse_sample(broken)

    def test_missing_retained_memory_and_failed_process_fail(self):
        for broken in [output().replace(' 1024 client_bytes', ''),
                       output().replace(' 80 identity_state_bytes', ''),
                       output().replace('1024 client_bytes', '0 client_bytes'),
                       output().replace('PASS', 'FAIL')]:
            with self.subTest(broken=broken):
                with self.assertRaises(ValueError):
                    measure.parse_sample(broken)


if __name__ == '__main__':
    unittest.main()

import tempfile
import unittest
from pathlib import Path
from ai_usage.__main__ import db_open, observe, latest, recommendation, defaults

class MvpTests(unittest.TestCase):
    def test_observation_and_rotation(self):
        with tempfile.TemporaryDirectory() as directory:
            con = db_open(Path(directory) / 'usage.db')
            observe(con, 'zai-codeplus', 100, 'limit-hit')
            observe(con, 'claude-pro', 42, 'manual')
            states = latest(con)
            self.assertEqual(states['zai-codeplus']['percent'], 100)
            self.assertIn('claude-pro', recommendation(defaults(), states))

    def test_rejects_invalid_percent(self):
        with tempfile.TemporaryDirectory() as directory:
            con = db_open(Path(directory) / 'usage.db')
            with self.assertRaises(ValueError):
                observe(con, 'ollama-pro', 101, 'manual')

if __name__ == '__main__':
    unittest.main()

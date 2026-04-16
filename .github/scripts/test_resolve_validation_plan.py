import unittest
import json
import os
import subprocess
from pathlib import Path

class TestResolveValidationPlan(unittest.TestCase):
    def test_resolve_full_all(self):
        # Correct path to the script
        script_path = Path(__file__).parent / "resolve_validation_plan.py"
        
        # Run the script with profile=full and lane-set=all
        result = subprocess.run(
            ["python3", str(script_path), "lab", "--profile", "full", "--lane-set", "all"],
            capture_output=True,
            text=True,
            check=True
        )
        
        # Parse the JSON output
        output = json.loads(result.stdout)
        
        # Basic validation
        self.assertIn("planned_matrix", output)
        self.assertIn("selected_matrix", output)
        self.assertIsInstance(output["planned_matrix"], dict)
        self.assertIsInstance(output["planned_matrix"]["include"], list)

if __name__ == "__main__":
    unittest.main()

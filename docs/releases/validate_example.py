"""Offline consistency walkthrough for the synthetic release, not a release gate.

Uses only the standard library. Does not execute WASM, contact a network, resolve
Git refs, or prove storage/client compatibility. Run from any working directory.
"""

import copy
import hashlib
import json
import re
from pathlib import Path


def require(condition, message):
    if not condition:
        raise ValueError(message)


def validate(example):
    require(example["example_only"] is True, "synthetic example marker required")
    require(example["format_version"] == 1, "unsupported manifest format")
    version = example["version"]
    # This walkthrough uses one fixed prerelease, not a general SemVer parser.
    require(version == "0.1.0-rc.1", "unexpected fixture release version")
    source = example["source"]
    require(source["tag"] == "contracts-v" + version, "tag/version mismatch")
    require(re.fullmatch(r"[0-9a-f]{40}", source["commit"]), "invalid source SHA")
    artifacts = {item["id"]: item for item in example["artifacts"]}
    require(len(artifacts) == len(example["artifacts"]), "duplicate artifact ID")
    require(set(artifacts) == {"access-control", "predifi-contract"}, "missing artifact")
    for artifact in artifacts.values():
        digest = hashlib.sha256(bytes.fromhex(artifact["fixture_wasm_hex"])).hexdigest()
        require(digest == artifact["sha256"], "artifact digest mismatch")
        require(digest == artifact["uploaded_wasm_hash"], "upload hash mismatch")
        require(artifact["storage_schema_version"] == 1, "unexpected fixture schema")
        require(artifact["readable_storage_schemas"] == [1], "unknown readable schema")

    seen = set()
    covered = set()
    for record in example["deployments"]:
        require(record["record_id"] not in seen, "duplicate deployment record")
        seen.add(record["record_id"])
        require(record["artifact_id"] in artifacts, "unknown deployment artifact")
        artifact = artifacts[record["artifact_id"]]
        covered.add(record["artifact_id"])
        require(record["release_tag"] == source["tag"], "deployment tag mismatch")
        require(record["source_commit"] == source["commit"], "deployment source mismatch")
        require(record["observed_wasm_hash"] == artifact["sha256"], "deployment hash mismatch")
        require(record["storage_schema_version"] == artifact["storage_schema_version"],
                "deployment schema mismatch")
        require(record["status"] == "synthetic-confirmed", "fixture must not claim live status")
        require(record["network_passphrase"] ==
                "PrediFi offline fixture network; NOT a live network", "wrong fixture network")
        require(re.fullmatch(r"C[A-Z2-7]{55}", record["contract_address"]), "address shape")
        require(re.fullmatch(r"[0-9a-f]{64}", record["transaction_hash"]), "transaction shape")
    require(covered == set(artifacts), "artifact has no deployment record")
    for client in example["clients"]:
        require(client["tested_contract_version"] == version, "untested client version")
        require(client["supported_contract_versions"] == [version],
                "unsupported client version")


def main():
    path = Path(__file__).with_name("contract-release.example.json")
    example = json.loads(path.read_text(encoding="utf-8"))
    validate(example)
    print("PASS: synthetic tag/source/artifact/schema/deployment/client mapping")
    cases = [
        ("artifacts", 0, "sha256", "0" * 64, "artifact digest mismatch"),
        ("deployments", 0, "artifact_id", "unknown", "unknown deployment artifact"),
        ("deployments", 0, "storage_schema_version", 2, "deployment schema mismatch"),
        ("deployments", 0, "source_commit", "2" * 40, "deployment source mismatch"),
        ("clients", 0, "supported_contract_versions", ["0.2.0"], "unsupported client version"),
    ]
    for section, index, key, value, expected in cases:
        changed = copy.deepcopy(example)
        changed[section][index][key] = value
        try:
            validate(changed)
        except ValueError as error:
            require(str(error) == expected, f"unexpected rejection: {error}")
            print(f"PASS: rejected {expected}")
        else:
            raise ValueError(f"failed to reject {expected}")
    print("No WASM executed and no network deployment performed.")


if __name__ == "__main__":
    main()

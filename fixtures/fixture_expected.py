"""Expected semantic contracts shared by deterministic fixture generators."""
import hashlib

def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()

def expected(
    outcome: str,
    description: str,
    semantic: str = "",
    error_code: str = "",
    limit: dict[str, object] | None = None,
) -> dict[str, object]:
    result: dict[str, object] = {
        "outcome": outcome,
        "error_code": error_code,
        "semantic_sha256": sha256(semantic.encode("utf-8")) if semantic else "",
        "description": description,
    }
    if limit is not None:
        result["limit"] = limit
    return result


def expected_hash(description: str, semantic_sha256: str) -> dict[str, object]:
    return {
        "outcome": "success",
        "error_code": "",
        "semantic_sha256": semantic_sha256,
        "description": description,
    }


def limit_expected(
    description: str,
    option: str,
    failing_value: int,
    passing_value: int,
    error_limit: str,
    passing_semantic: str,
    passing_semantic_sha256: str = "",
) -> dict[str, object]:
    return expected(
        "error",
        description,
        error_code="resourceLimit",
        limit={
            "option": option,
            "failing_value": failing_value,
            "passing_value": passing_value,
            "error_limit": error_limit,
            "passing_semantic_sha256": passing_semantic_sha256 or sha256(passing_semantic.encode("utf-8")),
        },
    )

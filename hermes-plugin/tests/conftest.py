"""Hermetic test env: no LLM, no network.

remember() takes an LLM-distill path when CAUSAL_MEMORY_LLM_API/KEY or
DEEPSEEK_API_KEY is set (Distiller::from_env falls back to the DeepSeek
key). In a keyed shell that makes sync_turn tests hit the network AND
environment-dependent: a junk turn distills to "nothing worth remembering"
and lands nothing, while the keyless raw fallback stores the text verbatim
(test_shutdown_drains_and_reinit_works asserts on that). These tests are
documented as offline — clear the keys so both paths are deterministic.
"""

import pytest


@pytest.fixture(autouse=True)
def _no_llm_env(monkeypatch):
    for var in (
        "CAUSAL_MEMORY_LLM_API",
        "CAUSAL_MEMORY_LLM_KEY",
        "CAUSAL_MEMORY_LLM_MODEL",
        "DEEPSEEK_API_KEY",
    ):
        monkeypatch.delenv(var, raising=False)

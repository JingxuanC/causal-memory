"""Hermes memory-provider plugin backed by causal-memory.

Implements the `agent.memory_provider.MemoryProvider` ABC surface (verified
against the Hermes developer guide). Hermes is an OPTIONAL import — the
module loads standalone so the plugin can be developed and tested without a
Hermes installation (the provider is duck-typed against the same surface).
"""

from .provider import CausalMemoryProvider, register

__all__ = ["CausalMemoryProvider", "register"]

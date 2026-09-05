"""Application logs never retain HTTP wire diagnostics or configured secrets."""

import logging
import re

from .config import config


class SafeLogFilter(logging.Filter):
    def filter(self, record):
        if record.name.startswith(("httpx", "httpcore")):
            return False
        message = record.getMessage()
        for value in (config.gemini_api_key, config.siliconflow_api_key):
            if value:
                message = message.replace(value, "[REDACTED]")
        message = re.sub(r"(?i)([?&](?:key|api_key|token)=)[^\s&\"']+", r"\1[REDACTED]", message)
        record.msg, record.args = message, ()
        return True


def configure_logging():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s %(message)s")
    for handler in logging.getLogger().handlers:
        if not any(isinstance(f, SafeLogFilter) for f in handler.filters):
            handler.addFilter(SafeLogFilter())
    for name in ("httpx", "httpx2", "httpcore"):
        logger = logging.getLogger(name)
        logger.setLevel(logging.WARNING)
        logger.addFilter(SafeLogFilter())

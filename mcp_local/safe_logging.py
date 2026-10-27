"""只接受事件分類，不接收路徑、查詢、供應商輸出或金鑰。"""
import logging
from logging.handlers import RotatingFileHandler


def create_logger(folder):
    folder.mkdir(parents=True, exist_ok=True)
    logger = logging.getLogger('mcp-local-background')
    logger.setLevel(logging.INFO)
    logger.propagate = False
    if not logger.handlers:
        handler = RotatingFileHandler(folder / 'background.log', maxBytes=1024 * 1024,
                                      backupCount=2, encoding='utf-8')
        handler.setFormatter(logging.Formatter('%(asctime)s %(message)s'))
        logger.addHandler(handler)
    return logger

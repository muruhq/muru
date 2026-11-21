import os

from mysite.settings import *

DEBUG = False
ALLOWED_HOSTS = ["127.0.0.1", "localhost"]
SECRET_KEY = os.environ["DJANGO_SECRET_KEY"]
STATIC_ROOT = os.environ.get("MURU_STATIC_DIR", BASE_DIR / "staticfiles")

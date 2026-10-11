"""A stand-in migration tool: `up` creates the schema, `down` drops it. Reads DATABASE_URL."""
import os
import sqlite3
import sys

path = os.environ["DATABASE_URL"].removeprefix("sqlite:///")
db = sqlite3.connect(path)
if sys.argv[1] == "up":
    db.executescript(
        "create table if not exists items(id integer primary key, name text not null);"
        "create table if not exists tags(id integer primary key, item_id integer references items(id), tag text);"
    )
else:
    db.executescript("drop table if exists tags; drop table if exists items;")
db.commit()
db.close()

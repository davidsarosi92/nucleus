"""A standard MongoDB driver (pymongo) against `nucleus server --mongo-compat`
for scripts/mongo-smoke.sh (PLAN.md §M16 Q5): CRUD and queries, the types
BSON has beyond JSON, the errors drivers act on.  Its one argument: the
connection string."""
import datetime as dt
import sys

import pymongo
from bson import Decimal128, ObjectId
from pymongo.errors import DuplicateKeyError, OperationFailure

url = sys.argv[1]
c = pymongo.MongoClient(url, serverSelectionTimeoutMS=3000)
db = c.shop
db.orders.drop()
o = db.orders
r = o.insert_one({"item": "pen", "qty": 3, "price": Decimal128("1.25"), "at": dt.datetime(2024, 5, 1, 12, 0)})
assert isinstance(r.inserted_id, ObjectId)
o.insert_many([{"_id": i, "item": f"i{i}", "qty": i, "tags": ["a", "b"][: i % 3]} for i in range(1, 300)])
assert o.count_documents({}) == 300
assert o.count_documents({"qty": {"$gte": 100}}) == 200
assert o.estimated_document_count() == 300
got = list(o.find({"qty": {"$lt": 5}}, {"_id": 0, "item": 1}).sort([("qty", -1)]).skip(1).limit(2))
assert got in ([{"item": "i3"}, {"item": "pen"}], [{"item": "pen"}, {"item": "i3"}]), got
assert len(list(o.find({}, batch_size=50))) == 300  # getMore
pen = o.find_one({"item": "pen"})
assert pen["price"] == Decimal128("1.25") and pen["at"] == dt.datetime(2024, 5, 1, 12, 0), pen
assert o.find_one({"_id": pen["_id"]})["item"] == "pen"  # ObjectId equality
assert o.find_one({"at": {"$gte": dt.datetime(2024, 1, 1)}})["item"] == "pen"  # date range
assert o.count_documents({"item": {"$regex": "^i1"}}) == 111
u = o.update_many({"qty": {"$gt": 290}}, {"$inc": {"qty": 1}, "$push": {"tags": "z"}})
assert (u.matched_count, u.modified_count) == (9, 9), (u.matched_count, u.modified_count)
u = o.update_one({"item": "new"}, {"$set": {"qty": 0}}, upsert=True)
assert isinstance(u.upserted_id, ObjectId), u.upserted_id
u = o.replace_one({"_id": 1}, {"item": "one", "qty": 1})
assert u.modified_count == 1
try:
    o.insert_one({"_id": 2})
    raise SystemExit("no duplicate error")
except DuplicateKeyError as e:
    assert e.code == 11000
r = o.bulk_write([pymongo.InsertOne({"_id": 1000}), pymongo.UpdateOne({"_id": 1000}, {"$set": {"x": 1}}),
                  pymongo.DeleteOne({"_id": 3})])
assert (r.inserted_count, r.modified_count, r.deleted_count) == (1, 1, 1), r.bulk_api_result
doc = o.find_one_and_update({"_id": 4}, {"$set": {"qty": 44}}, return_document=pymongo.ReturnDocument.AFTER)
assert doc["qty"] == 44
o.create_index([("qty", pymongo.ASCENDING)])
assert o.delete_many({"qty": {"$lt": 10}}).deleted_count > 0
try:
    o.find_one({"$where": "1"})
    raise SystemExit("no error")
except OperationFailure as e:
    print("refused as expected:", e.code, e.details.get("errmsg"))
print("collections:", db.list_collection_names())
print("pymongo", pymongo.version, ": all checks passed")

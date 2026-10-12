from collections import OrderedDict


class LRU:
    def __init__(self, capacity):
        self.capacity = capacity
        self.data = OrderedDict()

    def get(self, key, default=None):
        if key in self.data:
            self.data.move_to_end(key)
            return self.data[key]
        return default

    def put(self, key, value):
        if key in self.data:
            self.data.move_to_end(key)
        self.data[key] = value
        if len(self.data) > self.capacity:
            self.data.popitem(last=False)

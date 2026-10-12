class Counter:
    def __init__(self):
        self.count = 0

    def increment(self, by=1):
        self.count += by
        return self.count

    def reset(self):
        old, self.count = self.count, 0
        return old

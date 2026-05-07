#ifndef TEST_FIXTURE_H
#define TEST_FIXTURE_H

#define MAX_SIZE 1024

namespace demo {

class Base {
public:
    Base() {}
    virtual ~Base() {}
    virtual void run() = 0;
    int value;
};

class Derived : public Base {
public:
    Derived() : Base() {}
    void run() override;
private:
    int extra_data;
};

enum Color {
    RED,
    GREEN,
    BLUE
};

int global_counter;

int compute(int x, int y);

} // namespace demo

#endif

#include "demo.hpp"
#include <cstdio>

namespace demo {

int global_counter = 0;

int compute(int x, int y) {
    int result = x + y;
    global_counter++;
    return result;
}

void Derived::run() {
    int local = compute(extra_data, value);
    printf("running: %d\n", local);
    global_counter++;
}

} // namespace demo

int main() {
    demo::Derived d;
    d.run();
    int c = demo::compute(42, 10);
    return c;
}

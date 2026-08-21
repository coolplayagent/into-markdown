#include <cstddef>

extern "C" std::size_t into_markdown_allocator_bridge();

int main() { return into_markdown_allocator_bridge() == 17 ? 0 : 1; }

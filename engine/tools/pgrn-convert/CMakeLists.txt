set(TARGET llama-pgrn-convert)
add_executable(${TARGET} pgrn-convert.cpp)
install(TARGETS ${TARGET} RUNTIME)
target_link_libraries(${TARGET} PRIVATE ggml ${CMAKE_THREAD_LIBS_INIT})
target_compile_features(${TARGET} PRIVATE cxx_std_17)
# Only for peregrine_io.h — header-only and dependency-free, so the tool stays
# standalone while sharing one implementation of the per-platform I/O hints with
# the engine instead of keeping a second copy in sync.
target_include_directories(${TARGET} PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/../../src)

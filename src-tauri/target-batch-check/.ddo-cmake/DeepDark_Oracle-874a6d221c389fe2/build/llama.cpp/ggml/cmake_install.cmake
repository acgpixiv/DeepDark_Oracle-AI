# Install script for directory: E:/项目/AI--deepaisee/vendor/llama.cpp/ggml

# Set the install prefix
if(NOT DEFINED CMAKE_INSTALL_PREFIX)
  set(CMAKE_INSTALL_PREFIX "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2")
endif()
string(REGEX REPLACE "/$" "" CMAKE_INSTALL_PREFIX "${CMAKE_INSTALL_PREFIX}")

# Set the install configuration name.
if(NOT DEFINED CMAKE_INSTALL_CONFIG_NAME)
  if(BUILD_TYPE)
    string(REGEX REPLACE "^[^A-Za-z0-9_]+" ""
           CMAKE_INSTALL_CONFIG_NAME "${BUILD_TYPE}")
  else()
    set(CMAKE_INSTALL_CONFIG_NAME "Release")
  endif()
  message(STATUS "Install configuration: \"${CMAKE_INSTALL_CONFIG_NAME}\"")
endif()

# Set the component getting installed.
if(NOT CMAKE_INSTALL_COMPONENT)
  if(COMPONENT)
    message(STATUS "Install component: \"${COMPONENT}\"")
    set(CMAKE_INSTALL_COMPONENT "${COMPONENT}")
  else()
    set(CMAKE_INSTALL_COMPONENT)
  endif()
endif()

# Is this installation the result of a crosscompile?
if(NOT DEFINED CMAKE_CROSSCOMPILING)
  set(CMAKE_CROSSCOMPILING "FALSE")
endif()

if(NOT CMAKE_INSTALL_LOCAL_ONLY)
  # Include the install script for the subdirectory.
  include("E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/src/cmake_install.cmake")
endif()

if(CMAKE_INSTALL_COMPONENT STREQUAL "Unspecified" OR NOT CMAKE_INSTALL_COMPONENT)
  if(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Dd][Ee][Bb][Uu][Gg])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/src/Debug/ggml.lib")
  elseif(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Rr][Ee][Ll][Ee][Aa][Ss][Ee])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/src/Release/ggml.lib")
  elseif(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Mm][Ii][Nn][Ss][Ii][Zz][Ee][Rr][Ee][Ll])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/src/MinSizeRel/ggml.lib")
  elseif(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Rr][Ee][Ll][Ww][Ii][Tt][Hh][Dd][Ee][Bb][Ii][Nn][Ff][Oo])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/src/RelWithDebInfo/ggml.lib")
  endif()
endif()

if(CMAKE_INSTALL_COMPONENT STREQUAL "Unspecified" OR NOT CMAKE_INSTALL_COMPONENT)
  file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/include" TYPE FILE FILES
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-cpu.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-alloc.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-backend.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-blas.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-cann.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-cpp.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-cuda.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-opt.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-metal.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-rpc.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-virtgpu.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-sycl.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-vulkan.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-webgpu.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-zendnn.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/ggml-openvino.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/ggml/include/gguf.h"
    )
endif()

if(CMAKE_INSTALL_COMPONENT STREQUAL "Unspecified" OR NOT CMAKE_INSTALL_COMPONENT)
  if(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Dd][Ee][Bb][Uu][Gg])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/src/Debug/ggml-base.lib")
  elseif(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Rr][Ee][Ll][Ee][Aa][Ss][Ee])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/src/Release/ggml-base.lib")
  elseif(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Mm][Ii][Nn][Ss][Ii][Zz][Ee][Rr][Ee][Ll])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/src/MinSizeRel/ggml-base.lib")
  elseif(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Rr][Ee][Ll][Ww][Ii][Tt][Hh][Dd][Ee][Bb][Ii][Nn][Ff][Oo])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/src/RelWithDebInfo/ggml-base.lib")
  endif()
endif()

if(CMAKE_INSTALL_COMPONENT STREQUAL "Unspecified" OR NOT CMAKE_INSTALL_COMPONENT)
  file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib/cmake/ggml" TYPE FILE FILES
    "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/ggml-config.cmake"
    "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/ggml-version.cmake"
    )
endif()

string(REPLACE ";" "\n" CMAKE_INSTALL_MANIFEST_CONTENT
       "${CMAKE_INSTALL_MANIFEST_FILES}")
if(CMAKE_INSTALL_LOCAL_ONLY)
  file(WRITE "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/install_local_manifest.txt"
     "${CMAKE_INSTALL_MANIFEST_CONTENT}")
endif()

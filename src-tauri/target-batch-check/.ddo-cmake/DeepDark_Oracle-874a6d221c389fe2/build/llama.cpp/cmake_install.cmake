# Install script for directory: E:/项目/AI--deepaisee/vendor/llama.cpp

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
  include("E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/ggml/cmake_install.cmake")
endif()

if(NOT CMAKE_INSTALL_LOCAL_ONLY)
  # Include the install script for the subdirectory.
  include("E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/src/cmake_install.cmake")
endif()

if(NOT CMAKE_INSTALL_LOCAL_ONLY)
  # Include the install script for the subdirectory.
  include("E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/tools/mtmd/cmake_install.cmake")
endif()

if(CMAKE_INSTALL_COMPONENT STREQUAL "Unspecified" OR NOT CMAKE_INSTALL_COMPONENT)
  if(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Dd][Ee][Bb][Uu][Gg])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/src/Debug/llama.lib")
  elseif(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Rr][Ee][Ll][Ee][Aa][Ss][Ee])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/src/Release/llama.lib")
  elseif(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Mm][Ii][Nn][Ss][Ii][Zz][Ee][Rr][Ee][Ll])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/src/MinSizeRel/llama.lib")
  elseif(CMAKE_INSTALL_CONFIG_NAME MATCHES "^([Rr][Ee][Ll][Ww][Ii][Tt][Hh][Dd][Ee][Bb][Ii][Nn][Ff][Oo])$")
    file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib" TYPE STATIC_LIBRARY FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/src/RelWithDebInfo/llama.lib")
  endif()
endif()

if(CMAKE_INSTALL_COMPONENT STREQUAL "Unspecified" OR NOT CMAKE_INSTALL_COMPONENT)
  file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/include" TYPE FILE FILES
    "E:/项目/AI--deepaisee/vendor/llama.cpp/include/llama.h"
    "E:/项目/AI--deepaisee/vendor/llama.cpp/include/llama-cpp.h"
    )
endif()

if(CMAKE_INSTALL_COMPONENT STREQUAL "Unspecified" OR NOT CMAKE_INSTALL_COMPONENT)
  file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib/cmake/llama" TYPE FILE FILES
    "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/llama-config.cmake"
    "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/llama-version.cmake"
    )
endif()

if(CMAKE_INSTALL_COMPONENT STREQUAL "Unspecified" OR NOT CMAKE_INSTALL_COMPONENT)
  file(INSTALL DESTINATION "${CMAKE_INSTALL_PREFIX}/lib/pkgconfig" TYPE FILE FILES "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/llama.pc")
endif()

string(REPLACE ";" "\n" CMAKE_INSTALL_MANIFEST_CONTENT
       "${CMAKE_INSTALL_MANIFEST_FILES}")
if(CMAKE_INSTALL_LOCAL_ONLY)
  file(WRITE "E:/项目/AI--deepaisee/src-tauri/target-batch-check/.ddo-cmake/DeepDark_Oracle-874a6d221c389fe2/build/llama.cpp/install_local_manifest.txt"
     "${CMAKE_INSTALL_MANIFEST_CONTENT}")
endif()

# Proto generated files

Most of the files in this directory are generated duing the build from the proto files.

The generated files are added to the project directory so we can keep track of their versions and make builds simpler.
The alternative approach requires using `include!` on the build directory, which doesn't play well with IDEs.

See [build.rs](../../build.rs) for how this works.

# Snap Marketplace
Currently supports creating and buying listings, as well as redemption.
For an example run the service_provider_example.rs which sets up a listening server that also creates a listing advertising it's address. It will then listen for a redemption event and record the buyer's ip. The run client_example.rs providing the desired listing id as an argument, the client will buy and redeem the listing and establish a tcp connection to the server. The server should respond with "authorized" to the client.

